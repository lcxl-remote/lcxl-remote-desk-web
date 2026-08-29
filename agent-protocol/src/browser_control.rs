//! Current-schema contracts for the built-in browser-control provider.
//!
//! The edge adapter owns either the paired Chrome extension connection or an
//! explicitly enabled development DevTools MCP connection. The model sees
//! only the closed semantic tool set declared here; adapter internals, browser
//! credentials, cookies, storage, network logs, and arbitrary script
//! execution are never part of the provider contract.

use std::{collections::BTreeSet, fmt};

use serde::{Deserialize, Serialize};
use url::Url;
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

use crate::{
    computer_use::{ObjectKind, ObjectRef},
    data_lineage::ContentRef,
};

pub const BROWSER_CONTROL_SCHEMA_VERSION: u16 = 1;
pub const MIN_CHROME_DEVTOOLS_MCP_MAJOR_VERSION: u16 = 144;
pub const MAX_BROWSER_ID_BYTES: usize = 256;
pub const MAX_BROWSER_VERSION_BYTES: usize = 64;
pub const MAX_BROWSER_REASON_BYTES: usize = 512;
pub const MAX_BROWSER_ELEMENTS: usize = 512;
pub const MAX_ACCESSIBLE_NAME_BYTES: usize = 1024;
pub const MAX_BROWSER_URL_BYTES: usize = 4096;
pub const MAX_BROWSER_FORM_FIELDS: usize = 64;
pub const MAX_BROWSER_FORM_VALUE_BYTES: usize = 64 * 1024;
pub const MAX_BROWSER_FORM_TOTAL_BYTES: usize = 256 * 1024;
pub const MAX_BROWSER_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_BROWSER_WAIT_MS: u32 = 30_000;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum BrowserEngineKind {
    ChromeExtension,
    ChromeDevtoolsMcp,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum BrowserOriginKind {
    Https,
    /// Plain HTTP is accepted only for a loopback development origin.
    HttpLoopback,
}

/// Canonical origin identity. Paths, query strings, fragments, and credentials
/// are deliberately excluded so they cannot leak through readiness metadata.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct BrowserOrigin {
    pub kind: BrowserOriginKind,
    pub host_ascii: String,
    pub port: u16,
}

impl BrowserOrigin {
    pub fn validate(&self) -> Result<(), BrowserControlContractError> {
        if self.port == 0 {
            return Err(BrowserControlContractError::InvalidOrigin);
        }
        validate_id("origin.host_ascii", &self.host_ascii, 253)?;
        if self.host_ascii != self.host_ascii.to_ascii_lowercase()
            || self.host_ascii.contains("..")
            || self.host_ascii.starts_with(['.', '-'])
            || self.host_ascii.ends_with(['.', '-'])
            || !self
                .host_ascii
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':'))
        {
            return Err(BrowserControlContractError::InvalidOrigin);
        }
        if self.kind == BrowserOriginKind::HttpLoopback
            && !matches!(self.host_ascii.as_str(), "localhost" | "127.0.0.1" | "::1")
        {
            return Err(BrowserControlContractError::InvalidOrigin);
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct BrowserAdapterRef {
    pub engine: BrowserEngineKind,
    pub device_id: String,
    pub os_session_id: String,
    pub browser_major_version: u16,
    pub browser_version: String,
    pub adapter_id: String,
    pub adapter_version: String,
    /// Opaque profile incarnation. It is not a filesystem path or Chrome
    /// profile name and changes when the connected profile/session changes.
    pub profile_incarnation: String,
    /// Bumped whenever the MCP connection or approved browser session changes.
    pub connection_revision: u64,
}

impl BrowserAdapterRef {
    pub fn validate(&self) -> Result<(), BrowserControlContractError> {
        if self.engine == BrowserEngineKind::ChromeDevtoolsMcp
            && self.browser_major_version < MIN_CHROME_DEVTOOLS_MCP_MAJOR_VERSION
        {
            return Err(BrowserControlContractError::UnsupportedBrowserVersion);
        }
        validate_id("adapter.device_id", &self.device_id, MAX_BROWSER_ID_BYTES)?;
        validate_id(
            "adapter.os_session_id",
            &self.os_session_id,
            MAX_BROWSER_ID_BYTES,
        )?;
        validate_id(
            "adapter.browser_version",
            &self.browser_version,
            MAX_BROWSER_VERSION_BYTES,
        )?;
        validate_id("adapter.adapter_id", &self.adapter_id, MAX_BROWSER_ID_BYTES)?;
        validate_id(
            "adapter.adapter_version",
            &self.adapter_version,
            MAX_BROWSER_VERSION_BYTES,
        )?;
        validate_id(
            "adapter.profile_incarnation",
            &self.profile_incarnation,
            MAX_BROWSER_ID_BYTES,
        )?;
        if self.connection_revision == 0 {
            return Err(BrowserControlContractError::InvalidRevision);
        }
        Ok(())
    }
}

/// Closed, audited surface exposed by the generic browser Provider.
/// Intentionally absent: script evaluation, network/performance inspection,
/// cookies/storage/history, downloads, arbitrary tab enumeration, and raw
/// vision actions.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum BrowserToolKind {
    OpenPage,
    NavigatePage,
    TakeSnapshot,
    WaitFor,
    FillForm,
    UploadFile,
    ActivateElement,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum BrowserReadinessReason {
    UnsupportedBrowserVersion,
    ExtensionUnavailable,
    PairingRequired,
    HostPermissionMissing,
    RemoteDebuggingDisabled,
    UserApprovalRequired,
    UserDenied,
    McpUnavailable,
    Disconnected,
    InteractiveSessionLocked,
    ProfileChanged,
    Busy,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct BrowserReadiness {
    pub schema_version: u16,
    pub adapter: BrowserAdapterRef,
    /// The selected edge adapter is installed and locally enabled.
    pub adapter_enabled: bool,
    /// The owner completed the adapter-specific authorization step: extension
    /// pairing for the product path or Chrome approval for development MCP.
    pub user_authorized: bool,
    pub connected: bool,
    /// Informational session state. A live adapter may remain usable while the
    /// desktop is locked, so this field is not itself an execution fence.
    pub interactive_session_unlocked: bool,
    pub tools: Vec<BrowserToolKind>,
    pub reason: Option<BrowserReadinessReason>,
    pub observed_at_unix_ms: u64,
}

impl BrowserReadiness {
    pub fn validate(&self) -> Result<(), BrowserControlContractError> {
        validate_schema(self.schema_version)?;
        self.adapter.validate()?;
        if self.observed_at_unix_ms == 0 {
            return Err(BrowserControlContractError::InvalidTimestamp);
        }
        if self.user_authorized && !self.adapter_enabled
            || self.connected && (!self.adapter_enabled || !self.user_authorized)
        {
            return Err(BrowserControlContractError::InvalidReadiness);
        }
        if self.connected {
            if self.tools.is_empty()
                || self.tools.windows(2).any(|pair| pair[0] >= pair[1])
                || self.reason.is_some()
            {
                return Err(BrowserControlContractError::InvalidReadiness);
            }
        } else {
            if !self.tools.is_empty() {
                return Err(BrowserControlContractError::InvalidReadiness);
            }
            self.reason
                .ok_or(BrowserControlContractError::MissingReadinessReason)?;
        }
        Ok(())
    }
}

/// Stable page identity for subsequent semantic actions. A navigation or MCP
/// reconnection creates a new revision/incarnation; stale element references
/// therefore fail closed.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct BrowserPageRef {
    pub schema_version: u16,
    pub adapter: BrowserAdapterRef,
    pub page_id: String,
    pub page_incarnation: String,
    pub origin: BrowserOrigin,
    pub document_revision: u64,
    pub url_sha256: String,
    pub observed_at_unix_ms: u64,
}

impl BrowserPageRef {
    pub fn validate(&self) -> Result<(), BrowserControlContractError> {
        validate_schema(self.schema_version)?;
        self.adapter.validate()?;
        validate_id("page.page_id", &self.page_id, MAX_BROWSER_ID_BYTES)?;
        validate_id(
            "page.page_incarnation",
            &self.page_incarnation,
            MAX_BROWSER_ID_BYTES,
        )?;
        self.origin.validate()?;
        if self.document_revision == 0 {
            return Err(BrowserControlContractError::InvalidRevision);
        }
        validate_sha256(&self.url_sha256)?;
        if self.observed_at_unix_ms == 0 {
            return Err(BrowserControlContractError::InvalidTimestamp);
        }
        Ok(())
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum BrowserElementRole {
    Button,
    Link,
    Textbox,
    Checkbox,
    Combobox,
    Option,
    Tab,
    Dialog,
    Generic,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct BrowserElementRef {
    pub page_id: String,
    pub page_incarnation: String,
    pub document_revision: u64,
    pub element_id: String,
    pub role: BrowserElementRole,
    pub accessible_name: String,
    /// Present only for bounded form-control read-back. Static page text is
    /// never carried here.
    pub value: Option<String>,
    pub element_revision: u64,
}

impl BrowserElementRef {
    pub fn validate_for_page(
        &self,
        page: &BrowserPageRef,
    ) -> Result<(), BrowserControlContractError> {
        page.validate()?;
        validate_id("element.element_id", &self.element_id, MAX_BROWSER_ID_BYTES)?;
        validate_text(
            "element.accessible_name",
            &self.accessible_name,
            MAX_ACCESSIBLE_NAME_BYTES,
        )?;
        if self
            .value
            .as_ref()
            .is_some_and(|value| value.len() > MAX_BROWSER_FORM_VALUE_BYTES)
        {
            return Err(BrowserControlContractError::OversizedField("element.value"));
        }
        if self.element_revision == 0
            || self.page_id != page.page_id
            || self.page_incarnation != page.page_incarnation
            || self.document_revision != page.document_revision
        {
            return Err(BrowserControlContractError::StaleElementReference);
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct BrowserSemanticSnapshot {
    pub schema_version: u16,
    pub page: BrowserPageRef,
    pub elements: Vec<BrowserElementRef>,
    pub truncated: bool,
    pub captured_at_unix_ms: u64,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct BrowserNavigationTarget {
    pub url: String,
    pub origin: BrowserOrigin,
}

impl BrowserNavigationTarget {
    pub fn validate(&self) -> Result<(), BrowserControlContractError> {
        validate_text("navigation.url", &self.url, MAX_BROWSER_URL_BYTES)?;
        self.origin.validate()?;
        let parsed = Url::parse(&self.url)
            .map_err(|_| BrowserControlContractError::InvalidNavigationTarget)?;
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.fragment().is_some()
        {
            return Err(BrowserControlContractError::InvalidNavigationTarget);
        }
        let parsed_kind = match parsed.scheme() {
            "https" => BrowserOriginKind::Https,
            "http" => BrowserOriginKind::HttpLoopback,
            _ => return Err(BrowserControlContractError::InvalidNavigationTarget),
        };
        let parsed_origin = BrowserOrigin {
            kind: parsed_kind,
            host_ascii: parsed
                .host_str()
                .ok_or(BrowserControlContractError::InvalidNavigationTarget)?
                .to_ascii_lowercase(),
            port: parsed
                .port_or_known_default()
                .ok_or(BrowserControlContractError::InvalidNavigationTarget)?,
        };
        parsed_origin.validate()?;
        if parsed_origin != self.origin {
            return Err(BrowserControlContractError::OriginMismatch);
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum BrowserWaitState {
    Present,
    Absent,
    Enabled,
    Disabled,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum BrowserMutationClass {
    /// A reviewed site adapter proved that the form operation only creates or
    /// changes an external draft. ExportData authorization is still separate.
    WriteExternalDraft,
    /// Generic browser input with unknown business effect. It must use the R3
    /// InputFallback policy and cannot be silently narrowed by the model.
    InputFallback,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BrowserActivationClass {
    WriteExternalDraft,
    InputFallback,
    /// Constructed only by a reviewed site adapter after exact semantic
    /// read-back. The digest is the immutable SendPayloadSnapshot authority.
    SendExternal {
        payload_sha256: String,
    },
}

impl BrowserActivationClass {
    fn validate(&self) -> Result<(), BrowserControlContractError> {
        if let Self::SendExternal { payload_sha256 } = self {
            validate_sha256(payload_sha256)?;
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct BrowserFormField {
    pub element: BrowserElementRef,
    /// Bounded transient value carried to the edge. Implementations must not
    /// persist it in readiness, inventory, or audit projections.
    pub value: String,
}

/// How the edge proved one authorized form value after a mutation. The
/// committed-text variant covers tokenizing controls (for example an email
/// recipient combobox) whose input is replaced by a chip after commit.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum BrowserFormReadbackKind {
    ControlValue,
    CommittedText,
}

/// Bounded evidence for one field from the exact authorized FillForm request.
/// `value` can only be copied from the authorized request after the raw edge
/// snapshot proves an exact match; arbitrary page text never enters this
/// contract. `container_element_id` is the nearest form ancestor when present
/// and lets reviewed site adapters bind tokenized values to sibling controls.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct BrowserFormFieldReadback {
    pub request_element_id: String,
    pub request_role: BrowserElementRole,
    pub request_accessible_name: String,
    pub source_element_id: String,
    pub container_element_id: Option<String>,
    pub kind: BrowserFormReadbackKind,
    pub value: String,
}

impl BrowserFormFieldReadback {
    fn validate(&self) -> Result<(), BrowserControlContractError> {
        validate_id(
            "form_readback.request_element_id",
            &self.request_element_id,
            MAX_BROWSER_ID_BYTES,
        )?;
        validate_text(
            "form_readback.request_accessible_name",
            &self.request_accessible_name,
            MAX_ACCESSIBLE_NAME_BYTES,
        )?;
        validate_id(
            "form_readback.source_element_id",
            &self.source_element_id,
            MAX_BROWSER_ID_BYTES,
        )?;
        if let Some(container_element_id) = &self.container_element_id {
            validate_id(
                "form_readback.container_element_id",
                container_element_id,
                MAX_BROWSER_ID_BYTES,
            )?;
        }
        validate_text(
            "form_readback.value",
            &self.value,
            MAX_BROWSER_FORM_VALUE_BYTES,
        )?;
        match self.kind {
            BrowserFormReadbackKind::ControlValue
                if self.source_element_id == self.request_element_id =>
            {
                Ok(())
            }
            BrowserFormReadbackKind::CommittedText
                if self.request_role == BrowserElementRole::Combobox
                    && self.source_element_id != self.request_element_id
                    && self.container_element_id.is_some() =>
            {
                Ok(())
            }
            _ => Err(BrowserControlContractError::InvalidForm),
        }
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum BrowserAction {
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
    /// One reviewed draft mutation that fills the exact requested fields and
    /// uploads one exact edge-issued immutable artifact under the same writer
    /// lease. The adapter must read the fields and visible attachment name
    /// back before reporting success.
    FillFormAndUpload {
        page: BrowserPageRef,
        fields: Vec<BrowserFormField>,
        upload_element: BrowserElementRef,
        file: ObjectRef,
        content: ContentRef,
        file_name: String,
        media_type: String,
        size_bytes: u64,
        digest_sha256: String,
        mutation_class: BrowserMutationClass,
    },
    UploadFile {
        page: BrowserPageRef,
        element: BrowserElementRef,
        file: ObjectRef,
        content: ContentRef,
        file_name: String,
        media_type: String,
        size_bytes: u64,
        digest_sha256: String,
        mutation_class: BrowserMutationClass,
    },
    ActivateElement {
        page: BrowserPageRef,
        element: BrowserElementRef,
        activation_class: BrowserActivationClass,
    },
}

impl BrowserAction {
    pub fn validate(&self) -> Result<(), BrowserControlContractError> {
        match self {
            Self::OpenPage { target } => target.validate(),
            Self::NavigatePage { page, target } => {
                page.validate()?;
                target.validate()
            }
            Self::TakeSnapshot { page, max_elements } => {
                page.validate()?;
                if *max_elements == 0 || usize::from(*max_elements) > MAX_BROWSER_ELEMENTS {
                    return Err(BrowserControlContractError::InvalidSnapshot);
                }
                Ok(())
            }
            Self::WaitFor {
                page,
                element,
                timeout_ms,
                ..
            } => {
                element.validate_for_page(page)?;
                if *timeout_ms == 0 || *timeout_ms > MAX_BROWSER_WAIT_MS {
                    return Err(BrowserControlContractError::InvalidTimeout);
                }
                Ok(())
            }
            Self::FillForm { page, fields, .. } | Self::FillFormAndUpload { page, fields, .. } => {
                page.validate()?;
                if fields.is_empty() || fields.len() > MAX_BROWSER_FORM_FIELDS {
                    return Err(BrowserControlContractError::InvalidForm);
                }
                let mut ids = BTreeSet::new();
                let mut total_bytes = 0usize;
                for field in fields {
                    field.element.validate_for_page(page)?;
                    validate_text("form.value", &field.value, MAX_BROWSER_FORM_VALUE_BYTES)?;
                    total_bytes = total_bytes
                        .checked_add(field.value.len())
                        .ok_or(BrowserControlContractError::InvalidForm)?;
                    if !ids.insert(field.element.element_id.as_str()) {
                        return Err(BrowserControlContractError::DuplicateElement);
                    }
                }
                if total_bytes > MAX_BROWSER_FORM_TOTAL_BYTES {
                    return Err(BrowserControlContractError::InvalidForm);
                }
                if let Self::FillFormAndUpload {
                    upload_element,
                    file,
                    content,
                    file_name,
                    media_type,
                    size_bytes,
                    digest_sha256,
                    ..
                } = self
                {
                    validate_upload(
                        page,
                        upload_element,
                        file,
                        content,
                        file_name,
                        media_type,
                        *size_bytes,
                        digest_sha256,
                    )?;
                }
                Ok(())
            }
            Self::UploadFile {
                page,
                element,
                file,
                content,
                file_name,
                media_type,
                size_bytes,
                digest_sha256,
                ..
            } => validate_upload(
                page,
                element,
                file,
                content,
                file_name,
                media_type,
                *size_bytes,
                digest_sha256,
            ),
            Self::ActivateElement {
                page,
                element,
                activation_class,
            } => {
                element.validate_for_page(page)?;
                activation_class.validate()
            }
        }
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct BrowserActionRequest {
    pub schema_version: u16,
    pub call_id: String,
    pub action: BrowserAction,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum BrowserActionOutcome {
    PageOpened,
    PageNavigated,
    SnapshotCaptured,
    WaitSatisfied,
    FormFilled,
    FormFilledWithFile,
    FileUploaded,
    ElementActivated,
}

/// Edge-projected result. Raw MCP text, page titles, arbitrary tab inventory,
/// and upstream implementation details never cross this contract. `PageOpened`
/// may carry the first bounded semantic snapshot so the model never has to
/// round-trip a long provider-owned page reference merely to observe the page
/// it just opened.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct BrowserActionResult {
    pub schema_version: u16,
    pub call_id: String,
    pub outcome: BrowserActionOutcome,
    pub page: BrowserPageRef,
    pub snapshot: Option<BrowserSemanticSnapshot>,
    /// Present only for FormFilled. Each item is exact, request-bounded edge
    /// evidence and never a projection of arbitrary page text.
    pub form_readback: Vec<BrowserFormFieldReadback>,
    pub completed_at_unix_ms: u64,
}

impl BrowserActionResult {
    pub fn validate(&self) -> Result<(), BrowserControlContractError> {
        validate_schema(self.schema_version)?;
        validate_id("result.call_id", &self.call_id, MAX_BROWSER_ID_BYTES)?;
        self.page.validate()?;
        if self.completed_at_unix_ms == 0 {
            return Err(BrowserControlContractError::InvalidTimestamp);
        }
        let requires_snapshot = matches!(
            self.outcome,
            BrowserActionOutcome::SnapshotCaptured
                | BrowserActionOutcome::WaitSatisfied
                | BrowserActionOutcome::FormFilled
                | BrowserActionOutcome::FormFilledWithFile
                | BrowserActionOutcome::FileUploaded
                | BrowserActionOutcome::ElementActivated
        );
        let permits_snapshot =
            requires_snapshot || self.outcome == BrowserActionOutcome::PageOpened;
        if requires_snapshot && self.snapshot.is_none()
            || !permits_snapshot && self.snapshot.is_some()
        {
            return Err(BrowserControlContractError::InvalidActionResult);
        }
        if matches!(
            self.outcome,
            BrowserActionOutcome::FormFilled | BrowserActionOutcome::FormFilledWithFile
        ) {
            if self.form_readback.is_empty() || self.form_readback.len() > MAX_BROWSER_FORM_FIELDS {
                return Err(BrowserControlContractError::InvalidActionResult);
            }
            let mut request_ids = BTreeSet::new();
            let mut source_ids = BTreeSet::new();
            let mut total_bytes = 0usize;
            for readback in &self.form_readback {
                readback.validate()?;
                if !request_ids.insert(readback.request_element_id.as_str())
                    || !source_ids.insert(readback.source_element_id.as_str())
                {
                    return Err(BrowserControlContractError::DuplicateElement);
                }
                total_bytes = total_bytes
                    .checked_add(readback.value.len())
                    .ok_or(BrowserControlContractError::InvalidForm)?;
            }
            if total_bytes > MAX_BROWSER_FORM_TOTAL_BYTES {
                return Err(BrowserControlContractError::InvalidForm);
            }
        } else if !self.form_readback.is_empty() {
            return Err(BrowserControlContractError::InvalidActionResult);
        }
        if let Some(snapshot) = &self.snapshot {
            snapshot.validate()?;
            if snapshot.page != self.page
                || snapshot.captured_at_unix_ms > self.completed_at_unix_ms
            {
                return Err(BrowserControlContractError::InvalidActionResult);
            }
        }
        Ok(())
    }
}

impl BrowserActionRequest {
    pub fn validate(&self) -> Result<(), BrowserControlContractError> {
        validate_schema(self.schema_version)?;
        validate_id("action.call_id", &self.call_id, MAX_BROWSER_ID_BYTES)?;
        self.action.validate()
    }
}

impl BrowserSemanticSnapshot {
    pub fn validate(&self) -> Result<(), BrowserControlContractError> {
        validate_schema(self.schema_version)?;
        self.page.validate()?;
        if self.elements.len() > MAX_BROWSER_ELEMENTS || self.captured_at_unix_ms == 0 {
            return Err(BrowserControlContractError::InvalidSnapshot);
        }
        let mut ids = BTreeSet::new();
        let mut total_bytes = 0usize;
        for element in &self.elements {
            element.validate_for_page(&self.page)?;
            total_bytes = total_bytes
                .checked_add(element.accessible_name.len())
                .and_then(|total| total.checked_add(element.value.as_ref().map_or(0, String::len)))
                .ok_or(BrowserControlContractError::InvalidSnapshot)?;
            if !ids.insert(element.element_id.as_str()) {
                return Err(BrowserControlContractError::DuplicateElement);
            }
        }
        if total_bytes > MAX_BROWSER_FORM_TOTAL_BYTES {
            return Err(BrowserControlContractError::InvalidSnapshot);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserControlContractError {
    UnsupportedSchemaVersion(u16),
    UnsupportedBrowserVersion,
    EmptyField(&'static str),
    OversizedField(&'static str),
    InvalidText(&'static str),
    InvalidOrigin,
    InvalidNavigationTarget,
    OriginMismatch,
    InvalidRevision,
    InvalidTimestamp,
    InvalidDigest,
    InvalidReadiness,
    MissingReadinessReason,
    StaleElementReference,
    DuplicateElement,
    InvalidSnapshot,
    InvalidTimeout,
    InvalidForm,
    InvalidUpload,
    UnsafeFileName,
    InvalidActionResult,
}

impl fmt::Display for BrowserControlContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion(version) => {
                write!(
                    formatter,
                    "unsupported browser-control schema version: {version}"
                )
            }
            Self::UnsupportedBrowserVersion => {
                formatter.write_str("Chrome version does not support DevTools MCP auto-connect")
            }
            Self::EmptyField(field) => write!(formatter, "{field} must not be empty"),
            Self::OversizedField(field) => write!(formatter, "{field} is too long"),
            Self::InvalidText(field) => write!(formatter, "{field} contains invalid text"),
            Self::InvalidOrigin => formatter.write_str("browser origin is not canonical or safe"),
            Self::InvalidNavigationTarget => {
                formatter.write_str("browser navigation target is invalid")
            }
            Self::OriginMismatch => formatter.write_str("browser navigation origin mismatch"),
            Self::InvalidRevision => formatter.write_str("browser revision must be non-zero"),
            Self::InvalidTimestamp => formatter.write_str("browser timestamp is invalid"),
            Self::InvalidDigest => formatter.write_str("digest must be lowercase sha256"),
            Self::InvalidReadiness => formatter.write_str("browser readiness is inconsistent"),
            Self::MissingReadinessReason => {
                formatter.write_str("unready browser provider requires a reason")
            }
            Self::StaleElementReference => {
                formatter.write_str("browser element reference is stale")
            }
            Self::DuplicateElement => formatter.write_str("browser snapshot contains a duplicate"),
            Self::InvalidSnapshot => formatter.write_str("browser snapshot is invalid"),
            Self::InvalidTimeout => formatter.write_str("browser wait timeout is invalid"),
            Self::InvalidForm => formatter.write_str("browser form input is invalid"),
            Self::InvalidUpload => formatter.write_str("browser upload is invalid"),
            Self::UnsafeFileName => formatter.write_str("browser upload name is unsafe"),
            Self::InvalidActionResult => {
                formatter.write_str("browser action result is inconsistent")
            }
        }
    }
}

impl std::error::Error for BrowserControlContractError {}

fn validate_schema(version: u16) -> Result<(), BrowserControlContractError> {
    if version != BROWSER_CONTROL_SCHEMA_VERSION {
        return Err(BrowserControlContractError::UnsupportedSchemaVersion(
            version,
        ));
    }
    Ok(())
}

fn validate_id(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), BrowserControlContractError> {
    validate_text(field, value, max_bytes)?;
    if value.chars().any(|character| character.is_control()) {
        return Err(BrowserControlContractError::InvalidText(field));
    }
    Ok(())
}

fn validate_text(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), BrowserControlContractError> {
    if value.is_empty() {
        return Err(BrowserControlContractError::EmptyField(field));
    }
    if value.len() > max_bytes {
        return Err(BrowserControlContractError::OversizedField(field));
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<(), BrowserControlContractError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(BrowserControlContractError::InvalidDigest);
    }
    Ok(())
}

fn validate_safe_leaf_name(value: &str) -> Result<(), BrowserControlContractError> {
    validate_id("upload.file_name", value, MAX_BROWSER_ID_BYTES)?;
    if value == "."
        || value == ".."
        || value.contains(['/', '\\'])
        || value.ends_with(['.', ' '])
        || value.contains(':')
    {
        return Err(BrowserControlContractError::UnsafeFileName);
    }
    Ok(())
}

fn validate_upload(
    page: &BrowserPageRef,
    element: &BrowserElementRef,
    file: &ObjectRef,
    content: &ContentRef,
    file_name: &str,
    media_type: &str,
    size_bytes: u64,
    digest_sha256: &str,
) -> Result<(), BrowserControlContractError> {
    element.validate_for_page(page)?;
    if file.object_kind != ObjectKind::File
        || file.token.is_empty()
        || file.snapshot_id.is_empty()
        || file.expires_at.is_empty()
    {
        return Err(BrowserControlContractError::InvalidUpload);
    }
    validate_safe_leaf_name(file_name)?;
    validate_id("upload.media_type", media_type, MAX_BROWSER_ID_BYTES)?;
    validate_sha256(digest_sha256)?;
    if size_bytes == 0 || size_bytes > MAX_BROWSER_FILE_BYTES {
        return Err(BrowserControlContractError::InvalidUpload);
    }
    content
        .validate()
        .map_err(|_| BrowserControlContractError::InvalidUpload)?;
    match content {
        ContentRef::Artifact {
            artifact_id,
            sha256,
            size_bytes: content_size,
            media_type: content_media_type,
            ..
        } if artifact_id == &file.token
            && sha256 == digest_sha256
            && *content_size == size_bytes
            && content_media_type == media_type =>
        {
            Ok(())
        }
        _ => Err(BrowserControlContractError::InvalidUpload),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> BrowserAdapterRef {
        BrowserAdapterRef {
            engine: BrowserEngineKind::ChromeDevtoolsMcp,
            device_id: "device-1".into(),
            os_session_id: "session-1".into(),
            browser_major_version: 144,
            browser_version: "144.0.7559.0".into(),
            adapter_id: "chrome-devtools-mcp".into(),
            adapter_version: "1.7.0".into(),
            profile_incarnation: "profile-incarnation-1".into(),
            connection_revision: 7,
        }
    }

    fn page() -> BrowserPageRef {
        BrowserPageRef {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            adapter: adapter(),
            page_id: "page-1".into(),
            page_incarnation: "page-incarnation-1".into(),
            origin: BrowserOrigin {
                kind: BrowserOriginKind::Https,
                host_ascii: "mail.google.com".into(),
                port: 443,
            },
            document_revision: 4,
            url_sha256: "a".repeat(64),
            observed_at_unix_ms: 42,
        }
    }

    #[test]
    fn connected_readiness_accepts_only_sorted_closed_tool_surface() {
        let readiness = BrowserReadiness {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            adapter: adapter(),
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
                BrowserToolKind::UploadFile,
                BrowserToolKind::ActivateElement,
            ],
            reason: None,
            observed_at_unix_ms: 42,
        };
        readiness.validate().unwrap();
        let mut locked_but_connected = readiness.clone();
        locked_but_connected.interactive_session_unlocked = false;
        locked_but_connected.validate().unwrap();

        let raw_tool = r#""evaluate_script""#;
        assert!(serde_json::from_str::<BrowserToolKind>(raw_tool).is_err());
    }

    #[test]
    fn old_chrome_and_unapproved_connections_fail_closed() {
        let mut old = adapter();
        old.browser_major_version = 143;
        assert_eq!(
            old.validate(),
            Err(BrowserControlContractError::UnsupportedBrowserVersion)
        );

        let readiness = BrowserReadiness {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            adapter: adapter(),
            adapter_enabled: true,
            user_authorized: false,
            connected: true,
            interactive_session_unlocked: true,
            tools: vec![BrowserToolKind::TakeSnapshot],
            reason: None,
            observed_at_unix_ms: 42,
        };
        assert_eq!(
            readiness.validate(),
            Err(BrowserControlContractError::InvalidReadiness)
        );
    }

    #[test]
    fn plain_http_is_loopback_only() {
        BrowserOrigin {
            kind: BrowserOriginKind::HttpLoopback,
            host_ascii: "127.0.0.1".into(),
            port: 5174,
        }
        .validate()
        .unwrap();
        assert_eq!(
            BrowserOrigin {
                kind: BrowserOriginKind::HttpLoopback,
                host_ascii: "example.com".into(),
                port: 80,
            }
            .validate(),
            Err(BrowserControlContractError::InvalidOrigin)
        );
    }

    #[test]
    fn element_refs_are_bound_to_page_and_document_revision() {
        let page = page();
        let element = BrowserElementRef {
            page_id: page.page_id.clone(),
            page_incarnation: page.page_incarnation.clone(),
            document_revision: page.document_revision,
            element_id: "element-1".into(),
            role: BrowserElementRole::Textbox,
            accessible_name: "To".into(),
            value: None,
            element_revision: 1,
        };
        element.validate_for_page(&page).unwrap();

        let mut stale = element;
        stale.document_revision -= 1;
        assert_eq!(
            stale.validate_for_page(&page),
            Err(BrowserControlContractError::StaleElementReference)
        );
    }

    #[test]
    fn navigation_is_bound_to_canonical_origin_and_rejects_fragments() {
        BrowserNavigationTarget {
            url: "https://mail.google.com/mail/u/0/#inbox".into(),
            origin: page().origin,
        }
        .validate()
        .unwrap_err();

        assert_eq!(
            BrowserNavigationTarget {
                url: "https://example.com/compose".into(),
                origin: BrowserOrigin {
                    kind: BrowserOriginKind::Https,
                    host_ascii: "mail.google.com".into(),
                    port: 443,
                },
            }
            .validate(),
            Err(BrowserControlContractError::OriginMismatch)
        );
    }

    #[test]
    fn generic_activate_cannot_smuggle_send_authority() {
        let page = page();
        let element = BrowserElementRef {
            page_id: page.page_id.clone(),
            page_incarnation: page.page_incarnation.clone(),
            document_revision: page.document_revision,
            element_id: "send".into(),
            role: BrowserElementRole::Button,
            accessible_name: "Send".into(),
            value: None,
            element_revision: 1,
        };
        BrowserActionRequest {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            call_id: "call-1".into(),
            action: BrowserAction::ActivateElement {
                page: page.clone(),
                element: element.clone(),
                activation_class: BrowserActivationClass::InputFallback,
            },
        }
        .validate()
        .unwrap();

        let invalid_send = BrowserAction::ActivateElement {
            page,
            element,
            activation_class: BrowserActivationClass::SendExternal {
                payload_sha256: "not-a-digest".into(),
            },
        };
        assert_eq!(
            invalid_send.validate(),
            Err(BrowserControlContractError::InvalidDigest)
        );
    }

    #[test]
    fn upload_accepts_only_the_same_edge_artifact_identity_and_digest() {
        let page = page();
        let element = BrowserElementRef {
            page_id: page.page_id.clone(),
            page_incarnation: page.page_incarnation.clone(),
            document_revision: page.document_revision,
            element_id: "attachment-input".into(),
            role: BrowserElementRole::Button,
            accessible_name: "Attach files".into(),
            value: None,
            element_revision: 1,
        };
        let file = ObjectRef {
            token: "artifact-token-1".into(),
            snapshot_id: "worker-1:7".into(),
            object_kind: ObjectKind::File,
            expires_at: "2026-08-29T06:00:00Z".into(),
        };
        let action = BrowserAction::UploadFile {
            page,
            element,
            file: file.clone(),
            content: ContentRef::Artifact {
                artifact_id: file.token.clone(),
                sha256: "b".repeat(64),
                size_bytes: 42,
                media_type: "application/test".into(),
            },
            file_name: "report.docx".into(),
            media_type: "application/test".into(),
            size_bytes: 42,
            digest_sha256: "b".repeat(64),
            mutation_class: BrowserMutationClass::WriteExternalDraft,
        };
        action.validate().unwrap();

        let mut mismatched = action;
        if let BrowserAction::UploadFile { content, .. } = &mut mismatched {
            *content = ContentRef::Artifact {
                artifact_id: "different-artifact".into(),
                sha256: "b".repeat(64),
                size_bytes: 42,
                media_type: "application/test".into(),
            };
        }
        assert_eq!(
            mismatched.validate(),
            Err(BrowserControlContractError::InvalidUpload)
        );
    }

    #[test]
    fn result_never_accepts_raw_or_mismatched_snapshot_state() {
        let page = page();
        let result = BrowserActionResult {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            call_id: "call-1".into(),
            outcome: BrowserActionOutcome::SnapshotCaptured,
            page: page.clone(),
            snapshot: Some(BrowserSemanticSnapshot {
                schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
                page: page.clone(),
                elements: vec![],
                truncated: false,
                captured_at_unix_ms: 42,
            }),
            form_readback: Vec::new(),
            completed_at_unix_ms: 43,
        };
        result.validate().unwrap();

        let mut opened_with_snapshot = result.clone();
        opened_with_snapshot.outcome = BrowserActionOutcome::PageOpened;
        opened_with_snapshot.validate().unwrap();

        let mut navigated_with_snapshot = result.clone();
        navigated_with_snapshot.outcome = BrowserActionOutcome::PageNavigated;
        assert_eq!(
            navigated_with_snapshot.validate(),
            Err(BrowserControlContractError::InvalidActionResult)
        );

        let mut missing_snapshot = result.clone();
        missing_snapshot.snapshot = None;
        assert_eq!(
            missing_snapshot.validate(),
            Err(BrowserControlContractError::InvalidActionResult)
        );

        let mut mismatched = result;
        mismatched.snapshot.as_mut().unwrap().page.document_revision += 1;
        assert_eq!(
            mismatched.validate(),
            Err(BrowserControlContractError::InvalidActionResult)
        );
    }
}
