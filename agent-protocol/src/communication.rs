//! Current-schema contracts for first-party edge communication providers.
//!
//! The account session remains inside the controlled endpoint's application or
//! browser profile. A draft/handoff is not send authority. Sending is only
//! permitted from a sealed [`SendPayloadSnapshot`] whose digest is checked by
//! the central runtime and bound to an exact, one-shot `SendExternal` grant.

use std::{collections::BTreeSet, fmt};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

use crate::{
    browser_control::{
        BrowserElementRef, BrowserElementRole, BrowserOrigin, BrowserOriginKind, BrowserPageRef,
    },
    computer_use::CreatedFileArtifactOutput,
    data_lineage::ContentRef,
};

pub const COMMUNICATION_SCHEMA_VERSION: u16 = 3;
pub const MAX_COMMUNICATION_ID_BYTES: usize = 256;
pub const MAX_RECIPIENTS: usize = 64;
pub const MAX_GROUP_MEMBERS: usize = 256;
pub const MAX_ATTACHMENTS: usize = 32;
pub const MAX_SUBJECT_BYTES: usize = 998;
pub const MAX_ADDRESS_BYTES: usize = 512;
pub const MAX_DISPLAY_NAME_BYTES: usize = 512;
pub const MAX_BODY_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_ATTACHMENT_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_TOTAL_ATTACHMENT_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_LOCAL_DRAFT_BODY_BYTES: usize = 64 * 1024;
pub const MAX_LOCAL_DRAFT_ATTACHMENT_LABELS: usize = 32;
pub const GMAIL_WEB_HOST: &str = "mail.google.com";
pub const GMAIL_WEB_PORT: u16 = 443;
pub const SLACK_WEB_HOST: &str = "app.slack.com";
pub const SLACK_WEB_PORT: u16 = 443;

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
pub enum CommunicationChannel {
    Email,
    Chat,
    LocalDraft,
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
pub enum CommunicationSurfaceKind {
    ClassicOutlookDesktop,
    /// WebView2-based Outlook (new). The initial adapter may open a compose
    /// handoff but cannot claim semantic read-back or AI send authority.
    OutlookNewDesktop,
    /// Generic paired LCXL Chrome extension Provider. Gmail, Slack, and
    /// future sites are semantic adapters above this browser surface.
    ChromeExtension,
    /// Generic controlled-edge Chrome DevTools MCP Provider. Gmail, Slack,
    /// and future sites may use it only in explicitly enabled development mode.
    ChromeDevtoolsMcp,
    /// Built-in UIA/vision assistance. It may prepare a visible compose UI but
    /// can never claim exact read-back or AI send authority.
    AssistiveUi,
}

impl CommunicationSurfaceKind {
    fn supports_channel(self, channel: CommunicationChannel) -> bool {
        match self {
            Self::ClassicOutlookDesktop | Self::OutlookNewDesktop => {
                channel == CommunicationChannel::Email
            }
            Self::ChromeExtension | Self::ChromeDevtoolsMcp => {
                matches!(
                    channel,
                    CommunicationChannel::Email | CommunicationChannel::Chat
                )
            }
            Self::AssistiveUi => channel != CommunicationChannel::LocalDraft,
        }
    }

    fn supports_exact_send(self) -> bool {
        matches!(
            self,
            Self::ClassicOutlookDesktop | Self::ChromeExtension | Self::ChromeDevtoolsMcp
        )
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommunicationSurfaceScope {
    WebOrigin { origin: BrowserOrigin },
    DesktopApplication { application_id: String },
}

impl CommunicationSurfaceScope {
    fn validate(&self) -> Result<(), CommunicationContractError> {
        match self {
            Self::WebOrigin { origin } => origin
                .validate()
                .map_err(|_| CommunicationContractError::InvalidSurfaceScope),
            Self::DesktopApplication { application_id } => {
                validate_id("surface.application_id", application_id)
            }
        }
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct CommunicationSurfaceRef {
    pub channel: CommunicationChannel,
    pub kind: CommunicationSurfaceKind,
    /// Stable application/origin scope used by readiness and exact grants.
    /// It deliberately excludes a browser path, query, fragment, or token.
    pub scope: CommunicationSurfaceScope,
    pub device_id: String,
    pub os_session_id: String,
    pub adapter_id: String,
    pub adapter_version: String,
    /// Opaque application/browser profile identity. It is not a filesystem
    /// profile path and must not contain a cookie, token, or credential.
    pub profile_id: String,
    /// Adapter-owned stable identity for the account currently displayed by
    /// the logged-in application. It is not a login secret.
    pub account_id: String,
    /// Bumped whenever the OS session, application/profile, account identity,
    /// adapter connection, or semantic readiness changes.
    pub revision: u64,
}

impl CommunicationSurfaceRef {
    pub fn validate(&self) -> Result<(), CommunicationContractError> {
        if self.channel == CommunicationChannel::LocalDraft
            || !self.kind.supports_channel(self.channel)
        {
            return Err(CommunicationContractError::SurfaceChannelMismatch);
        }
        self.scope.validate()?;
        if !matches!(
            (&self.kind, &self.scope),
            (
                CommunicationSurfaceKind::ClassicOutlookDesktop,
                CommunicationSurfaceScope::DesktopApplication { .. }
            ) | (
                CommunicationSurfaceKind::OutlookNewDesktop,
                CommunicationSurfaceScope::DesktopApplication { .. }
            ) | (
                CommunicationSurfaceKind::ChromeExtension,
                CommunicationSurfaceScope::WebOrigin { .. }
            ) | (
                CommunicationSurfaceKind::ChromeDevtoolsMcp,
                CommunicationSurfaceScope::WebOrigin { .. }
            ) | (CommunicationSurfaceKind::AssistiveUi, _)
        ) {
            return Err(CommunicationContractError::InvalidSurfaceScope);
        }
        validate_id("surface.device_id", &self.device_id)?;
        validate_id("surface.os_session_id", &self.os_session_id)?;
        validate_id("surface.adapter_id", &self.adapter_id)?;
        validate_id("surface.adapter_version", &self.adapter_version)?;
        validate_id("surface.profile_id", &self.profile_id)?;
        validate_id("surface.account_id", &self.account_id)?;
        if self.revision == 0 {
            return Err(CommunicationContractError::InvalidRevision);
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct CommunicationSurfaceReadiness {
    pub schema_version: u16,
    pub surface: CommunicationSurfaceRef,
    pub installed: bool,
    pub running: bool,
    pub authenticated: bool,
    pub compose_ready: bool,
    pub semantic_readback: bool,
    pub exact_send_eligible: bool,
    pub reason: Option<String>,
    pub observed_at_unix_ms: u64,
}

impl CommunicationSurfaceReadiness {
    pub fn validate(&self) -> Result<(), CommunicationContractError> {
        validate_schema(self.schema_version)?;
        self.surface.validate()?;
        if self.observed_at_unix_ms == 0 {
            return Err(CommunicationContractError::InvalidTimestamp);
        }
        if self.running && !self.installed
            || self.authenticated && !self.running
            || self.compose_ready && !self.authenticated
            || self.semantic_readback && !self.compose_ready
            || self.exact_send_eligible
                && (!self.semantic_readback || !self.surface.kind.supports_exact_send())
        {
            return Err(CommunicationContractError::InvalidReadiness);
        }
        if self.compose_ready {
            if self.reason.is_some() {
                return Err(CommunicationContractError::UnexpectedReadinessReason);
            }
        } else {
            validate_text(
                "surface_readiness.reason",
                self.reason
                    .as_deref()
                    .ok_or(CommunicationContractError::MissingReadinessReason)?,
                MAX_DISPLAY_NAME_BYTES,
            )?;
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
pub enum RecipientRole {
    To,
    Cc,
    Bcc,
    ChatDestination,
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
pub enum RecipientKind {
    EmailMailbox,
    ChatUser,
    ChatChannel,
    ChatGroup,
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
pub enum RecipientDisplayWarning {
    UnicodeAddress,
    MixedAsciiAndNonAscii,
    BidirectionalOrInvisibleControl,
}

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
pub struct ResolvedRecipientMember {
    pub stable_id: String,
    pub canonical_address: String,
}

impl ResolvedRecipientMember {
    fn validate(&self) -> Result<(), CommunicationContractError> {
        validate_id("member.stable_id", &self.stable_id)?;
        validate_address("member.canonical_address", &self.canonical_address)
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct RecipientIdentity {
    pub role: RecipientRole,
    pub kind: RecipientKind,
    /// Connector-owned immutable mailbox/user/channel/group identity.
    pub stable_id: String,
    /// Server-canonicalized address retained for exact UI review and sealing.
    pub canonical_address: String,
    pub display_name: Option<String>,
    /// Fixed warning tokens computed by trusted canonicalization logic. They
    /// are sealed with the recipient identity and rendered by local UI text.
    pub display_warnings: Vec<RecipientDisplayWarning>,
    /// Empty for a direct recipient. A group must contain the resolved,
    /// deterministic member snapshot that the edge adapter promises to recheck
    /// immediately before dispatch.
    pub resolved_members: Vec<ResolvedRecipientMember>,
    pub member_snapshot_sha256: Option<String>,
}

impl RecipientIdentity {
    pub fn validate(&self) -> Result<(), CommunicationContractError> {
        validate_id("recipient.stable_id", &self.stable_id)?;
        validate_address("recipient.canonical_address", &self.canonical_address)?;
        if let Some(display_name) = &self.display_name {
            validate_text(
                "recipient.display_name",
                display_name,
                MAX_DISPLAY_NAME_BYTES,
            )?;
        }
        if self.display_warnings.len() > 3
            || self
                .display_warnings
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            return Err(CommunicationContractError::NonCanonicalDisplayWarnings);
        }
        if self.resolved_members.len() > MAX_GROUP_MEMBERS {
            return Err(CommunicationContractError::TooManyItems("resolved_members"));
        }
        match self.kind {
            RecipientKind::ChatGroup => {
                if self.resolved_members.is_empty() {
                    return Err(CommunicationContractError::MissingGroupSnapshot);
                }
                validate_sha256(
                    self.member_snapshot_sha256
                        .as_deref()
                        .ok_or(CommunicationContractError::MissingGroupSnapshot)?,
                )?;
                let mut previous = None;
                for member in &self.resolved_members {
                    member.validate()?;
                    let key = (&member.stable_id, &member.canonical_address);
                    if previous.as_ref().is_some_and(|value| value >= &key) {
                        return Err(CommunicationContractError::NonCanonicalMembers);
                    }
                    previous = Some(key);
                }
            }
            _ => {
                if !self.resolved_members.is_empty() || self.member_snapshot_sha256.is_some() {
                    return Err(CommunicationContractError::UnexpectedGroupSnapshot);
                }
            }
        }
        match self.kind {
            RecipientKind::EmailMailbox if matches!(self.role, RecipientRole::ChatDestination) => {
                Err(CommunicationContractError::RecipientRoleMismatch)
            }
            RecipientKind::ChatUser | RecipientKind::ChatChannel | RecipientKind::ChatGroup
                if !matches!(self.role, RecipientRole::ChatDestination) =>
            {
                Err(CommunicationContractError::RecipientRoleMismatch)
            }
            _ => Ok(()),
        }
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ImmutableBodySnapshot {
    pub content: ContentRef,
    pub media_type: String,
    pub size_bytes: u64,
    pub digest_sha256: String,
}

impl ImmutableBodySnapshot {
    pub fn validate(&self) -> Result<(), CommunicationContractError> {
        validate_immutable_content_ref(
            &self.content,
            &self.media_type,
            self.size_bytes,
            &self.digest_sha256,
            MAX_BODY_BYTES,
        )
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ImmutableAttachmentSnapshot {
    pub content: ContentRef,
    pub file_name: String,
    pub media_type: String,
    pub size_bytes: u64,
    pub digest_sha256: String,
}

impl ImmutableAttachmentSnapshot {
    pub fn validate(&self) -> Result<(), CommunicationContractError> {
        validate_safe_leaf_name(&self.file_name)?;
        validate_immutable_content_ref(
            &self.content,
            &self.media_type,
            self.size_bytes,
            &self.digest_sha256,
            MAX_ATTACHMENT_BYTES,
        )
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct CommunicationPayload {
    pub surface: CommunicationSurfaceRef,
    pub recipients: Vec<RecipientIdentity>,
    pub subject: String,
    pub body: ImmutableBodySnapshot,
    pub attachments: Vec<ImmutableAttachmentSnapshot>,
}

impl CommunicationPayload {
    pub fn validate(&self) -> Result<(), CommunicationContractError> {
        self.surface.validate()?;
        validate_text("subject", &self.subject, MAX_SUBJECT_BYTES)?;
        self.body.validate()?;
        if self.recipients.is_empty() || self.recipients.len() > MAX_RECIPIENTS {
            return Err(CommunicationContractError::InvalidRecipientCount);
        }
        if self.attachments.len() > MAX_ATTACHMENTS {
            return Err(CommunicationContractError::TooManyItems("attachments"));
        }

        let mut recipient_keys = BTreeSet::new();
        for recipient in &self.recipients {
            recipient.validate()?;
            if !recipient_keys.insert((
                recipient.role,
                recipient.kind,
                recipient.stable_id.as_str(),
            )) {
                return Err(CommunicationContractError::DuplicateItem("recipients"));
            }
        }
        match self.surface.channel {
            CommunicationChannel::Email => {
                if self
                    .recipients
                    .iter()
                    .any(|recipient| recipient.kind != RecipientKind::EmailMailbox)
                {
                    return Err(CommunicationContractError::ChannelMismatch);
                }
            }
            CommunicationChannel::Chat => {
                if self.recipients.len() != 1
                    || self.recipients[0].kind == RecipientKind::EmailMailbox
                {
                    return Err(CommunicationContractError::ChannelMismatch);
                }
            }
            CommunicationChannel::LocalDraft => {
                return Err(CommunicationContractError::LocalDraftCannotUseExternalPayload);
            }
        }

        let mut attachment_digests = BTreeSet::new();
        let mut total_attachment_bytes = 0u64;
        for attachment in &self.attachments {
            attachment.validate()?;
            if !attachment_digests.insert(attachment.digest_sha256.as_str()) {
                return Err(CommunicationContractError::DuplicateItem("attachments"));
            }
            total_attachment_bytes = total_attachment_bytes
                .checked_add(attachment.size_bytes)
                .ok_or(CommunicationContractError::AttachmentBudgetExceeded)?;
        }
        if total_attachment_bytes > MAX_TOTAL_ATTACHMENT_BYTES {
            return Err(CommunicationContractError::AttachmentBudgetExceeded);
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct CommunicationDraft {
    pub schema_version: u16,
    pub draft_id: String,
    pub run_id: String,
    pub payload: CommunicationPayload,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CommunicationPrepareVerification {
    SemanticExact,
    AssistiveUnverified,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CommunicationSendAuthority {
    ManualOnly,
    ExactGrantEligible,
}

/// A durable record that the edge stopped automation with a visible compose
/// surface ready for the user. This is never proof that a message was sent.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct CommunicationDraftHandoff {
    pub schema_version: u16,
    pub handoff_id: String,
    pub run_id: String,
    pub surface: CommunicationSurfaceRef,
    /// Opaque adapter-owned identity of the compose window/document. It must
    /// not be a browser URL containing credentials.
    pub compose_id: String,
    /// Digest of the immutable payload the central runtime asked the edge to
    /// prepare.
    pub prepared_payload_sha256: String,
    pub verification: CommunicationPrepareVerification,
    /// Present only after semantic read-back of all sealed fields.
    pub readback_payload_sha256: Option<String>,
    pub send_authority: CommunicationSendAuthority,
    pub handed_off_at_unix_ms: u64,
}

impl CommunicationDraftHandoff {
    pub fn validate(&self) -> Result<(), CommunicationContractError> {
        validate_schema(self.schema_version)?;
        validate_id("handoff_id", &self.handoff_id)?;
        validate_id("run_id", &self.run_id)?;
        self.surface.validate()?;
        validate_id("compose_id", &self.compose_id)?;
        validate_sha256(&self.prepared_payload_sha256)?;
        if self.handed_off_at_unix_ms == 0 {
            return Err(CommunicationContractError::InvalidTimestamp);
        }
        match self.verification {
            CommunicationPrepareVerification::SemanticExact => {
                let readback = self
                    .readback_payload_sha256
                    .as_deref()
                    .ok_or(CommunicationContractError::MissingReadbackDigest)?;
                validate_sha256(readback)?;
                if readback != self.prepared_payload_sha256 {
                    return Err(CommunicationContractError::ReadbackDigestMismatch);
                }
                if self.send_authority == CommunicationSendAuthority::ExactGrantEligible
                    && !self.surface.kind.supports_exact_send()
                {
                    return Err(CommunicationContractError::InvalidSendAuthority);
                }
            }
            CommunicationPrepareVerification::AssistiveUnverified => {
                if self.readback_payload_sha256.is_some()
                    || self.send_authority != CommunicationSendAuthority::ManualOnly
                {
                    return Err(CommunicationContractError::InvalidSendAuthority);
                }
            }
        }
        Ok(())
    }
}

/// A provider-neutral, local-only draft document. Recipient strings are
/// clearly unverified intent and carry no delivery authority; an external
/// edge adapter must resolve them into [`RecipientIdentity`] before any draft
/// or send operation can be authorized.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct LocalDraftRecipient {
    pub role: RecipientRole,
    pub address: String,
    pub display_name: Option<String>,
}

impl LocalDraftRecipient {
    pub fn validate(&self) -> Result<(), CommunicationContractError> {
        validate_address("local_recipient.address", &self.address)?;
        if let Some(display_name) = &self.display_name {
            validate_text(
                "local_recipient.display_name",
                display_name,
                MAX_DISPLAY_NAME_BYTES,
            )?;
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct LocalDraftDocument {
    pub schema_version: u16,
    pub recipients: Vec<LocalDraftRecipient>,
    pub subject: String,
    pub body_plain_text: String,
    pub attachment_labels: Vec<String>,
}

/// Exact model/tool input accepted before an Outlook (new) compose handoff is
/// sealed with server-owned run and surface metadata.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct OutlookNewDraftHandoffInput {
    pub draft: LocalDraftDocument,
}

impl OutlookNewDraftHandoffInput {
    pub fn validate(&self) -> Result<(), CommunicationContractError> {
        self.draft.validate()?;
        if !self.draft.attachment_labels.is_empty() {
            return Err(CommunicationContractError::TooManyItems(
                "outlook_new_handoff.attachments",
            ));
        }
        if self
            .draft
            .recipients
            .iter()
            .any(|recipient| recipient.role == RecipientRole::ChatDestination)
        {
            return Err(CommunicationContractError::ChannelMismatch);
        }
        Ok(())
    }
}

/// Exact input accepted by the Outlook (new) manual compose adapter.
///
/// The adapter intentionally accepts no attachment bytes or paths. It opens a
/// visible compose surface through the registered `mailto:` handler and then
/// stops. Since Outlook (new) does not expose stable semantic read-back here,
/// successful execution is always an assistive, manual-only handoff.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct OutlookNewComposeHandoffRequest {
    pub schema_version: u16,
    pub call_id: String,
    pub run_id: String,
    pub surface: CommunicationSurfaceRef,
    pub draft: LocalDraftDocument,
}

/// Exact model/tool input accepted by the reviewed Slack Web site adapter.
///
/// The page and composer references must come from the current bounded browser
/// snapshot. The adapter fills only this composer, performs semantic read-back,
/// and stops without activating Slack's send control.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct SlackWebDraftHandoffInput {
    pub schema_version: u16,
    pub page: BrowserPageRef,
    pub composer: BrowserElementRef,
    pub body_plain_text: String,
}

impl SlackWebDraftHandoffInput {
    pub fn validate(&self) -> Result<(), CommunicationContractError> {
        validate_schema(self.schema_version)?;
        self.page
            .validate()
            .map_err(|_| CommunicationContractError::InvalidSurfaceScope)?;
        if self.page.origin.kind != BrowserOriginKind::Https
            || self.page.origin.host_ascii != SLACK_WEB_HOST
            || self.page.origin.port != SLACK_WEB_PORT
        {
            return Err(CommunicationContractError::InvalidSurfaceScope);
        }
        self.composer
            .validate_for_page(&self.page)
            .map_err(|_| CommunicationContractError::InvalidSurfaceScope)?;
        if self.composer.role != BrowserElementRole::Textbox {
            return Err(CommunicationContractError::RecipientRoleMismatch);
        }
        validate_text(
            "slack.composer.accessible_name",
            &self.composer.accessible_name,
            MAX_DISPLAY_NAME_BYTES,
        )?;
        validate_plain_text_body(
            "slack.body_plain_text",
            &self.body_plain_text,
            MAX_LOCAL_DRAFT_BODY_BYTES,
        )
    }
}

/// Exact model/tool input accepted by the reviewed Gmail Web site adapter.
///
/// The three field references must come from one fresh bounded browser
/// snapshot. Gmail currently exposes the To recipient editor as either a
/// textbox or a combobox, while Subject and Message Body remain textboxes.
/// This initial semantic adapter supports one To recipient, a subject, and a
/// plain-text body, accepts at most one typed immutable artifact created by the
/// controlled edge, and never activates Gmail's Send control.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct GmailWebDraftHandoffInput {
    pub schema_version: u16,
    pub page: BrowserPageRef,
    pub to_field: BrowserElementRef,
    pub subject_field: BrowserElementRef,
    pub body_field: BrowserElementRef,
    pub attachment: Option<GmailWebAttachmentInput>,
    pub draft: LocalDraftDocument,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct GmailWebAttachmentInput {
    /// Fresh semantic reference for Gmail's reviewed file chooser control.
    pub element: BrowserElementRef,
    /// Exact edge-issued immutable artifact identity; never a native path.
    pub artifact: CreatedFileArtifactOutput,
}

impl GmailWebDraftHandoffInput {
    pub fn validate(&self) -> Result<(), CommunicationContractError> {
        validate_schema(self.schema_version)?;
        self.page
            .validate()
            .map_err(|_| CommunicationContractError::InvalidSurfaceScope)?;
        if self.page.origin.kind != BrowserOriginKind::Https
            || self.page.origin.host_ascii != GMAIL_WEB_HOST
            || self.page.origin.port != GMAIL_WEB_PORT
        {
            return Err(CommunicationContractError::InvalidSurfaceScope);
        }
        self.draft.validate()?;
        if self.draft.recipients.len() != 1 || self.draft.recipients[0].role != RecipientRole::To {
            return Err(CommunicationContractError::InvalidRecipientCount);
        }
        match &self.attachment {
            None if self.draft.attachment_labels.is_empty() => {}
            Some(attachment)
                if self.draft.attachment_labels.as_slice()
                    == [attachment.artifact.file_name.as_str()] =>
            {
                attachment
                    .element
                    .validate_for_page(&self.page)
                    .map_err(|_| CommunicationContractError::InvalidSurfaceScope)?;
                attachment
                    .artifact
                    .validate()
                    .map_err(|_| CommunicationContractError::InvalidContentRef)?;
            }
            _ => {
                return Err(CommunicationContractError::TooManyItems(
                    "gmail_web_handoff.attachments",
                ));
            }
        }
        let fields = [&self.to_field, &self.subject_field, &self.body_field];
        let mut element_ids = BTreeSet::new();
        for field in fields {
            field
                .validate_for_page(&self.page)
                .map_err(|_| CommunicationContractError::InvalidSurfaceScope)?;
            validate_text(
                "gmail.field.accessible_name",
                &field.accessible_name,
                MAX_DISPLAY_NAME_BYTES,
            )?;
            if !element_ids.insert(field.element_id.as_str()) {
                return Err(CommunicationContractError::DuplicateItem(
                    "gmail_web_handoff.fields",
                ));
            }
        }
        if self
            .attachment
            .as_ref()
            .is_some_and(|attachment| element_ids.contains(attachment.element.element_id.as_str()))
        {
            return Err(CommunicationContractError::DuplicateItem(
                "gmail_web_handoff.fields",
            ));
        }
        if !matches!(
            self.to_field.role,
            BrowserElementRole::Textbox | BrowserElementRole::Combobox
        ) || self.subject_field.role != BrowserElementRole::Textbox
            || self.body_field.role != BrowserElementRole::Textbox
        {
            return Err(CommunicationContractError::RecipientRoleMismatch);
        }
        Ok(())
    }
}

impl OutlookNewComposeHandoffRequest {
    pub fn validate(&self) -> Result<(), CommunicationContractError> {
        validate_schema(self.schema_version)?;
        validate_id("call_id", &self.call_id)?;
        validate_id("run_id", &self.run_id)?;
        self.surface.validate()?;
        if self.surface.kind != CommunicationSurfaceKind::OutlookNewDesktop
            || self.surface.channel != CommunicationChannel::Email
        {
            return Err(CommunicationContractError::SurfaceChannelMismatch);
        }
        OutlookNewDraftHandoffInput {
            draft: self.draft.clone(),
        }
        .validate()
    }
}

impl LocalDraftDocument {
    pub fn validate(&self) -> Result<(), CommunicationContractError> {
        validate_schema(self.schema_version)?;
        if self.recipients.is_empty() || self.recipients.len() > MAX_RECIPIENTS {
            return Err(CommunicationContractError::InvalidRecipientCount);
        }
        validate_text("subject", &self.subject, MAX_SUBJECT_BYTES)?;
        validate_plain_text_body(
            "body_plain_text",
            &self.body_plain_text,
            MAX_LOCAL_DRAFT_BODY_BYTES,
        )?;
        let mut recipients = BTreeSet::new();
        for recipient in &self.recipients {
            recipient.validate()?;
            if !recipients.insert((recipient.role, recipient.address.as_str())) {
                return Err(CommunicationContractError::DuplicateItem("recipients"));
            }
        }
        if self.attachment_labels.len() > MAX_LOCAL_DRAFT_ATTACHMENT_LABELS {
            return Err(CommunicationContractError::TooManyItems(
                "attachment_labels",
            ));
        }
        let mut labels = BTreeSet::new();
        for label in &self.attachment_labels {
            validate_text("attachment_label", label, MAX_ADDRESS_BYTES)?;
            if !labels.insert(label) {
                return Err(CommunicationContractError::DuplicateItem(
                    "attachment_labels",
                ));
            }
        }
        Ok(())
    }
}

impl CommunicationDraft {
    pub fn validate(&self) -> Result<(), CommunicationContractError> {
        validate_schema(self.schema_version)?;
        validate_id("draft_id", &self.draft_id)?;
        validate_id("run_id", &self.run_id)?;
        if self.created_at_unix_ms == 0 || self.updated_at_unix_ms < self.created_at_unix_ms {
            return Err(CommunicationContractError::InvalidTimestamp);
        }
        self.payload.validate()
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct SendPayloadSnapshot {
    pub schema_version: u16,
    pub snapshot_id: String,
    pub run_id: String,
    pub payload: CommunicationPayload,
    pub sealed_at_unix_ms: u64,
    /// SHA-256 of the canonical JSON bytes of all preceding snapshot fields.
    /// The hash construction lives in shared pure logic, not in an edge adapter.
    pub canonical_payload_sha256: String,
}

impl SendPayloadSnapshot {
    pub fn validate_shape(&self) -> Result<(), CommunicationContractError> {
        validate_schema(self.schema_version)?;
        validate_id("snapshot_id", &self.snapshot_id)?;
        validate_id("run_id", &self.run_id)?;
        if self.sealed_at_unix_ms == 0 {
            return Err(CommunicationContractError::InvalidTimestamp);
        }
        validate_sha256(&self.canonical_payload_sha256)?;
        self.payload.validate()
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SendOutcome {
    Sent,
    DefinitelyNotSent,
    OutcomeUnknown,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct SendReceipt {
    pub schema_version: u16,
    pub snapshot_id: String,
    pub snapshot_sha256: String,
    pub idempotency_key: String,
    pub outcome: SendOutcome,
    pub remote_message_id: Option<String>,
    pub observed_at_unix_ms: u64,
}

impl SendReceipt {
    pub fn validate(&self) -> Result<(), CommunicationContractError> {
        validate_schema(self.schema_version)?;
        validate_id("snapshot_id", &self.snapshot_id)?;
        validate_sha256(&self.snapshot_sha256)?;
        validate_id("idempotency_key", &self.idempotency_key)?;
        if self.observed_at_unix_ms == 0 {
            return Err(CommunicationContractError::InvalidTimestamp);
        }
        match self.outcome {
            SendOutcome::Sent => {
                validate_id(
                    "remote_message_id",
                    self.remote_message_id
                        .as_deref()
                        .ok_or(CommunicationContractError::MissingRemoteMessageId)?,
                )?;
            }
            SendOutcome::DefinitelyNotSent | SendOutcome::OutcomeUnknown => {
                if self.remote_message_id.is_some() {
                    return Err(CommunicationContractError::UnexpectedRemoteMessageId);
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommunicationContractError {
    UnsupportedSchemaVersion(u16),
    EmptyField(&'static str),
    OversizedField(&'static str),
    InvalidRevision,
    SurfaceChannelMismatch,
    InvalidSurfaceScope,
    InvalidReadiness,
    MissingReadinessReason,
    UnexpectedReadinessReason,
    InvalidTimestamp,
    InvalidDigest,
    InvalidContentRef,
    ContentRefMismatch,
    UnsafeFileName,
    TooManyItems(&'static str),
    DuplicateItem(&'static str),
    InvalidRecipientCount,
    MissingGroupSnapshot,
    UnexpectedGroupSnapshot,
    NonCanonicalMembers,
    NonCanonicalDisplayWarnings,
    RecipientRoleMismatch,
    ChannelMismatch,
    LocalDraftCannotUseExternalPayload,
    AttachmentBudgetExceeded,
    MissingReadbackDigest,
    ReadbackDigestMismatch,
    InvalidSendAuthority,
    MissingRemoteMessageId,
    UnexpectedRemoteMessageId,
}

impl fmt::Display for CommunicationContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion(version) => {
                write!(
                    formatter,
                    "unsupported communication schema version: {version}"
                )
            }
            Self::EmptyField(field) => write!(formatter, "{field} must not be empty"),
            Self::OversizedField(field) => write!(formatter, "{field} is too long"),
            Self::InvalidRevision => formatter.write_str("surface revision must be non-zero"),
            Self::SurfaceChannelMismatch => {
                formatter.write_str("communication surface and channel mismatch")
            }
            Self::InvalidSurfaceScope => {
                formatter.write_str("communication surface scope and kind mismatch")
            }
            Self::InvalidReadiness => {
                formatter.write_str("communication surface readiness is inconsistent")
            }
            Self::MissingReadinessReason => {
                formatter.write_str("unready communication surface requires a reason")
            }
            Self::UnexpectedReadinessReason => {
                formatter.write_str("ready communication surface cannot contain a reason")
            }
            Self::InvalidTimestamp => formatter.write_str("communication timestamp is invalid"),
            Self::InvalidDigest => formatter.write_str("digest must be lowercase sha256"),
            Self::InvalidContentRef => {
                formatter.write_str("communication content must be immutable")
            }
            Self::ContentRefMismatch => {
                formatter.write_str("communication content metadata does not match ContentRef")
            }
            Self::UnsafeFileName => formatter.write_str("attachment name must be a safe leaf name"),
            Self::TooManyItems(field) => write!(formatter, "{field} has too many items"),
            Self::DuplicateItem(field) => write!(formatter, "{field} contains a duplicate"),
            Self::InvalidRecipientCount => formatter.write_str("recipient count is invalid"),
            Self::MissingGroupSnapshot => formatter.write_str("group member snapshot is required"),
            Self::UnexpectedGroupSnapshot => {
                formatter.write_str("direct recipient cannot contain a group snapshot")
            }
            Self::NonCanonicalMembers => {
                formatter.write_str("group members must be strictly sorted and unique")
            }
            Self::NonCanonicalDisplayWarnings => {
                formatter.write_str("recipient display warnings must be sorted and unique")
            }
            Self::RecipientRoleMismatch => formatter.write_str("recipient role and kind mismatch"),
            Self::ChannelMismatch => formatter.write_str("account and recipient channel mismatch"),
            Self::LocalDraftCannotUseExternalPayload => {
                formatter.write_str("local draft cannot be used as an external send payload")
            }
            Self::AttachmentBudgetExceeded => {
                formatter.write_str("attachment byte budget exceeded")
            }
            Self::MissingReadbackDigest => {
                formatter.write_str("semantic handoff requires a read-back digest")
            }
            Self::ReadbackDigestMismatch => {
                formatter.write_str("semantic handoff read-back does not match prepared payload")
            }
            Self::InvalidSendAuthority => {
                formatter.write_str("communication handoff send authority is invalid")
            }
            Self::MissingRemoteMessageId => {
                formatter.write_str("sent receipt requires a remote message id")
            }
            Self::UnexpectedRemoteMessageId => {
                formatter.write_str("non-sent receipt cannot contain a remote message id")
            }
        }
    }
}

impl std::error::Error for CommunicationContractError {}

fn validate_schema(version: u16) -> Result<(), CommunicationContractError> {
    if version == COMMUNICATION_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(CommunicationContractError::UnsupportedSchemaVersion(
            version,
        ))
    }
}

fn validate_id(field: &'static str, value: &str) -> Result<(), CommunicationContractError> {
    validate_text(field, value, MAX_COMMUNICATION_ID_BYTES)
}

fn validate_address(field: &'static str, value: &str) -> Result<(), CommunicationContractError> {
    validate_text(field, value, MAX_ADDRESS_BYTES)
}

fn validate_text(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), CommunicationContractError> {
    if value.trim().is_empty() {
        Err(CommunicationContractError::EmptyField(field))
    } else if value.len() > max_bytes {
        Err(CommunicationContractError::OversizedField(field))
    } else if value.chars().any(char::is_control) {
        Err(CommunicationContractError::EmptyField(field))
    } else {
        Ok(())
    }
}

fn validate_plain_text_body(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), CommunicationContractError> {
    if value.trim().is_empty() {
        Err(CommunicationContractError::EmptyField(field))
    } else if value.len() > max_bytes {
        Err(CommunicationContractError::OversizedField(field))
    } else if value
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        // Requiring canonical LF line endings keeps the rendered artifact
        // deterministic while still allowing normal multi-line plain text.
        Err(CommunicationContractError::EmptyField(field))
    } else {
        Ok(())
    }
}

fn validate_sha256(value: &str) -> Result<(), CommunicationContractError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(CommunicationContractError::InvalidDigest)
    }
}

fn validate_safe_leaf_name(value: &str) -> Result<(), CommunicationContractError> {
    validate_text("attachment.file_name", value, MAX_ADDRESS_BYTES)?;
    if value == "."
        || value == ".."
        || value.ends_with(['.', ' '])
        || value.contains(['/', '\\', ':'])
    {
        return Err(CommunicationContractError::UnsafeFileName);
    }
    Ok(())
}

fn validate_immutable_content_ref(
    content: &ContentRef,
    media_type: &str,
    size_bytes: u64,
    digest_sha256: &str,
    max_size: u64,
) -> Result<(), CommunicationContractError> {
    content
        .validate()
        .map_err(|_| CommunicationContractError::InvalidContentRef)?;
    validate_id("media_type", media_type)?;
    validate_sha256(digest_sha256)?;
    if size_bytes == 0 || size_bytes > max_size {
        return Err(CommunicationContractError::InvalidContentRef);
    }
    let (content_digest, content_size, content_media_type) = match content {
        ContentRef::ImmutableBlob {
            sha256,
            size_bytes,
            media_type,
            ..
        }
        | ContentRef::Artifact {
            sha256,
            size_bytes,
            media_type,
            ..
        } => (sha256, size_bytes, media_type),
        ContentRef::EphemeralObservation { .. } => {
            return Err(CommunicationContractError::InvalidContentRef);
        }
    };
    if content_digest != digest_sha256
        || *content_size != size_bytes
        || content_media_type != media_type
    {
        return Err(CommunicationContractError::ContentRefMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser_control::{
        BROWSER_CONTROL_SCHEMA_VERSION, BrowserAdapterRef, BrowserEngineKind, BrowserOrigin,
    };

    fn digest(byte: char) -> String {
        byte.to_string().repeat(64)
    }

    fn body() -> ImmutableBodySnapshot {
        ImmutableBodySnapshot {
            content: ContentRef::ImmutableBlob {
                blob_id: "body-1".into(),
                sha256: digest('a'),
                size_bytes: 12,
                media_type: "text/plain".into(),
            },
            media_type: "text/plain".into(),
            size_bytes: 12,
            digest_sha256: digest('a'),
        }
    }

    fn email_payload() -> CommunicationPayload {
        CommunicationPayload {
            surface: CommunicationSurfaceRef {
                channel: CommunicationChannel::Email,
                kind: CommunicationSurfaceKind::ClassicOutlookDesktop,
                scope: CommunicationSurfaceScope::DesktopApplication {
                    application_id: "outlook_classic".into(),
                },
                device_id: "device-1".into(),
                os_session_id: "session-1".into(),
                adapter_id: "outlook-desktop-v1".into(),
                adapter_version: "1.0.0".into(),
                profile_id: "profile-1".into(),
                account_id: "account-1".into(),
                revision: 4,
            },
            recipients: vec![RecipientIdentity {
                role: RecipientRole::To,
                kind: RecipientKind::EmailMailbox,
                stable_id: "mailbox:alice".into(),
                canonical_address: "alice@example.com".into(),
                display_name: Some("Alice".into()),
                display_warnings: Vec::new(),
                resolved_members: Vec::new(),
                member_snapshot_sha256: None,
            }],
            subject: "Quarterly report".into(),
            body: body(),
            attachments: Vec::new(),
        }
    }

    fn slack_input() -> SlackWebDraftHandoffInput {
        let page = BrowserPageRef {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            adapter: BrowserAdapterRef {
                engine: BrowserEngineKind::ChromeDevtoolsMcp,
                device_id: "device-1".into(),
                os_session_id: "session-1".into(),
                browser_major_version: 151,
                browser_version: "151.0.0.0".into(),
                adapter_id: "chrome-devtools-mcp".into(),
                adapter_version: "1.7.0".into(),
                profile_incarnation: "profile-1".into(),
                connection_revision: 7,
            },
            page_id: "page-1".into(),
            page_incarnation: "page-incarnation-1".into(),
            origin: BrowserOrigin {
                kind: BrowserOriginKind::Https,
                host_ascii: SLACK_WEB_HOST.into(),
                port: SLACK_WEB_PORT,
            },
            document_revision: 2,
            url_sha256: digest('e'),
            observed_at_unix_ms: 42,
        };
        SlackWebDraftHandoffInput {
            schema_version: COMMUNICATION_SCHEMA_VERSION,
            composer: BrowserElementRef {
                page_id: page.page_id.clone(),
                page_incarnation: page.page_incarnation.clone(),
                document_revision: page.document_revision,
                element_id: "composer-1".into(),
                role: BrowserElementRole::Textbox,
                accessible_name: "Message #test".into(),
                value: None,
                element_revision: 1,
            },
            page,
            body_plain_text: "Stage 5 draft verification".into(),
        }
    }

    fn gmail_input() -> GmailWebDraftHandoffInput {
        let mut input_page = slack_input().page;
        input_page.origin.host_ascii = GMAIL_WEB_HOST.into();
        let field = |element_id: &str, accessible_name: &str| BrowserElementRef {
            page_id: input_page.page_id.clone(),
            page_incarnation: input_page.page_incarnation.clone(),
            document_revision: input_page.document_revision,
            element_id: element_id.into(),
            role: BrowserElementRole::Textbox,
            accessible_name: accessible_name.into(),
            value: None,
            element_revision: 1,
        };
        let mut to_field = field("to-1", "To recipients");
        to_field.role = BrowserElementRole::Combobox;
        GmailWebDraftHandoffInput {
            schema_version: COMMUNICATION_SCHEMA_VERSION,
            to_field,
            subject_field: field("subject-1", "Subject"),
            body_field: field("body-1", "Message Body"),
            page: input_page,
            attachment: None,
            draft: LocalDraftDocument {
                schema_version: COMMUNICATION_SCHEMA_VERSION,
                recipients: vec![LocalDraftRecipient {
                    role: RecipientRole::To,
                    address: "alice@example.com".into(),
                    display_name: None,
                }],
                subject: "Stage 5 Gmail verification".into(),
                body_plain_text: "Semantic draft only; do not send.".into(),
                attachment_labels: Vec::new(),
            },
        }
    }

    #[test]
    fn exact_email_payload_accepts_only_immutable_content() {
        email_payload().validate().unwrap();
        let mut payload = email_payload();
        payload.body.content = ContentRef::EphemeralObservation {
            observation_id: "screen".into(),
            size_bytes: 12,
            expires_at_unix_ms: 100,
        };
        assert_eq!(
            payload.validate(),
            Err(CommunicationContractError::InvalidContentRef)
        );
    }

    #[test]
    fn chrome_extension_surface_is_a_valid_web_origin_surface() {
        let surface = CommunicationSurfaceRef {
            channel: CommunicationChannel::Email,
            kind: CommunicationSurfaceKind::ChromeExtension,
            scope: CommunicationSurfaceScope::WebOrigin {
                origin: BrowserOrigin {
                    kind: BrowserOriginKind::Https,
                    host_ascii: GMAIL_WEB_HOST.into(),
                    port: GMAIL_WEB_PORT,
                },
            },
            device_id: "device-1".into(),
            os_session_id: "session-1".into(),
            adapter_id: "lcxl-browser-extension".into(),
            adapter_version: "0.1.0".into(),
            profile_id: "profile-1".into(),
            account_id: "gmail-web-current-profile".into(),
            revision: 1,
        };
        surface.validate().unwrap();
    }

    #[test]
    fn group_requires_sorted_stable_member_snapshot() {
        let group = RecipientIdentity {
            role: RecipientRole::ChatDestination,
            kind: RecipientKind::ChatGroup,
            stable_id: "group:ops".into(),
            canonical_address: "ops".into(),
            display_name: None,
            display_warnings: Vec::new(),
            resolved_members: vec![
                ResolvedRecipientMember {
                    stable_id: "user:b".into(),
                    canonical_address: "b".into(),
                },
                ResolvedRecipientMember {
                    stable_id: "user:a".into(),
                    canonical_address: "a".into(),
                },
            ],
            member_snapshot_sha256: Some(digest('b')),
        };
        assert_eq!(
            group.validate(),
            Err(CommunicationContractError::NonCanonicalMembers)
        );
    }

    #[test]
    fn receipt_makes_unknown_distinct_and_unretryable_by_contract() {
        let mut receipt = SendReceipt {
            schema_version: COMMUNICATION_SCHEMA_VERSION,
            snapshot_id: "snapshot-1".into(),
            snapshot_sha256: digest('c'),
            idempotency_key: "send:snapshot-1".into(),
            outcome: SendOutcome::OutcomeUnknown,
            remote_message_id: None,
            observed_at_unix_ms: 42,
        };
        receipt.validate().unwrap();
        receipt.remote_message_id = Some("invented".into());
        assert_eq!(
            receipt.validate(),
            Err(CommunicationContractError::UnexpectedRemoteMessageId)
        );
    }

    #[test]
    fn unknown_fields_fail_closed() {
        let json = r#"{
            "channel":"email",
            "kind":"classic_outlook_desktop",
            "scope":{"kind":"desktop_application","application_id":"outlook_classic"},
            "device_id":"device-1",
            "os_session_id":"session-1",
            "adapter_id":"outlook-desktop-v1",
            "adapter_version":"1.0.0",
            "profile_id":"profile-1",
            "account_id":"account-1",
            "revision":1,
            "token":"secret"
        }"#;
        assert!(serde_json::from_str::<CommunicationSurfaceRef>(json).is_err());
    }

    #[test]
    fn readiness_cannot_claim_exact_send_for_assistive_ui() {
        let readiness = CommunicationSurfaceReadiness {
            schema_version: COMMUNICATION_SCHEMA_VERSION,
            surface: CommunicationSurfaceRef {
                channel: CommunicationChannel::Email,
                kind: CommunicationSurfaceKind::AssistiveUi,
                scope: CommunicationSurfaceScope::DesktopApplication {
                    application_id: "windows_desktop".into(),
                },
                device_id: "device-1".into(),
                os_session_id: "session-1".into(),
                adapter_id: "windows-uia-v1".into(),
                adapter_version: "1.0.0".into(),
                profile_id: "desktop".into(),
                account_id: "visible-account".into(),
                revision: 1,
            },
            installed: true,
            running: true,
            authenticated: true,
            compose_ready: true,
            semantic_readback: true,
            exact_send_eligible: true,
            reason: None,
            observed_at_unix_ms: 42,
        };
        assert_eq!(
            readiness.validate(),
            Err(CommunicationContractError::InvalidReadiness)
        );
    }

    #[test]
    fn outlook_new_is_discoverable_but_cannot_claim_exact_send() {
        let readiness = CommunicationSurfaceReadiness {
            schema_version: COMMUNICATION_SCHEMA_VERSION,
            surface: CommunicationSurfaceRef {
                channel: CommunicationChannel::Email,
                kind: CommunicationSurfaceKind::OutlookNewDesktop,
                scope: CommunicationSurfaceScope::DesktopApplication {
                    application_id: "microsoft_outlook_for_windows".into(),
                },
                device_id: "device-1".into(),
                os_session_id: "session-1".into(),
                adapter_id: "outlook-new-mailto-v1".into(),
                adapter_version: "1.0.0".into(),
                profile_id: "profile-incarnation-1".into(),
                account_id: "visible-account".into(),
                revision: 1,
            },
            installed: true,
            running: true,
            authenticated: true,
            compose_ready: true,
            semantic_readback: true,
            exact_send_eligible: true,
            reason: None,
            observed_at_unix_ms: 42,
        };
        assert_eq!(
            readiness.validate(),
            Err(CommunicationContractError::InvalidReadiness)
        );
    }

    #[test]
    fn manual_handoff_is_distinct_from_send_receipt() {
        let handoff = CommunicationDraftHandoff {
            schema_version: COMMUNICATION_SCHEMA_VERSION,
            handoff_id: "handoff-1".into(),
            run_id: "run-1".into(),
            surface: email_payload().surface,
            compose_id: "compose-1".into(),
            prepared_payload_sha256: digest('d'),
            verification: CommunicationPrepareVerification::SemanticExact,
            readback_payload_sha256: Some(digest('d')),
            send_authority: CommunicationSendAuthority::ManualOnly,
            handed_off_at_unix_ms: 42,
        };
        handoff.validate().unwrap();

        let mut assistive = handoff;
        assistive.surface.kind = CommunicationSurfaceKind::AssistiveUi;
        assistive.verification = CommunicationPrepareVerification::AssistiveUnverified;
        assistive.readback_payload_sha256 = None;
        assistive.send_authority = CommunicationSendAuthority::ExactGrantEligible;
        assert_eq!(
            assistive.validate(),
            Err(CommunicationContractError::InvalidSendAuthority)
        );
    }

    #[test]
    fn local_draft_accepts_multiline_lf_body_but_rejects_controls() {
        let mut draft = LocalDraftDocument {
            schema_version: COMMUNICATION_SCHEMA_VERSION,
            recipients: vec![LocalDraftRecipient {
                role: RecipientRole::To,
                address: "alice@example.com".into(),
                display_name: Some("Alice".into()),
            }],
            subject: "Review".into(),
            body_plain_text: "First line\nSecond line\twith detail".into(),
            attachment_labels: Vec::new(),
        };
        draft.validate().unwrap();

        draft.body_plain_text = "First line\r\nSecond line".into();
        assert_eq!(
            draft.validate(),
            Err(CommunicationContractError::EmptyField("body_plain_text"))
        );
        draft.body_plain_text = "First line\0Second line".into();
        assert_eq!(
            draft.validate(),
            Err(CommunicationContractError::EmptyField("body_plain_text"))
        );
    }

    #[test]
    fn slack_web_handoff_binds_exact_site_page_and_composer() {
        let input = slack_input();
        input.validate().unwrap();

        let mut wrong_site = input.clone();
        wrong_site.page.origin.host_ascii = "example.com".into();
        assert_eq!(
            wrong_site.validate(),
            Err(CommunicationContractError::InvalidSurfaceScope)
        );

        let mut stale = input.clone();
        stale.composer.document_revision += 1;
        assert_eq!(
            stale.validate(),
            Err(CommunicationContractError::InvalidSurfaceScope)
        );

        let mut wrong_role = input.clone();
        wrong_role.composer.role = BrowserElementRole::Button;
        assert_eq!(
            wrong_role.validate(),
            Err(CommunicationContractError::RecipientRoleMismatch)
        );

        let mut unnamed_composer = input;
        unnamed_composer.composer.accessible_name.clear();
        assert_eq!(
            unnamed_composer.validate(),
            Err(CommunicationContractError::InvalidSurfaceScope)
        );
    }

    #[test]
    fn gmail_web_handoff_binds_one_exact_mail_and_three_fresh_fields() {
        let input = gmail_input();
        input.validate().unwrap();

        let mut wrong_site = input.clone();
        wrong_site.page.origin.host_ascii = "example.com".into();
        assert_eq!(
            wrong_site.validate(),
            Err(CommunicationContractError::InvalidSurfaceScope)
        );

        let mut stale = input.clone();
        stale.body_field.document_revision += 1;
        assert_eq!(
            stale.validate(),
            Err(CommunicationContractError::InvalidSurfaceScope)
        );

        let mut duplicate_field = input.clone();
        duplicate_field.body_field.element_id = duplicate_field.subject_field.element_id.clone();
        assert_eq!(
            duplicate_field.validate(),
            Err(CommunicationContractError::DuplicateItem(
                "gmail_web_handoff.fields"
            ))
        );

        let mut invalid_to_role = input.clone();
        invalid_to_role.to_field.role = BrowserElementRole::Generic;
        assert_eq!(
            invalid_to_role.validate(),
            Err(CommunicationContractError::RecipientRoleMismatch)
        );

        let mut invalid_subject_role = input.clone();
        invalid_subject_role.subject_field.role = BrowserElementRole::Combobox;
        assert_eq!(
            invalid_subject_role.validate(),
            Err(CommunicationContractError::RecipientRoleMismatch)
        );

        let mut with_attachment = input.clone();
        let mut upload_element = with_attachment.body_field.clone();
        upload_element.element_id = "attachment-1".into();
        upload_element.role = BrowserElementRole::Button;
        upload_element.accessible_name = "Attach files".into();
        let file = crate::computer_use::ObjectRef {
            token: "artifact-token-1".into(),
            snapshot_id: "worker-1:1".into(),
            object_kind: crate::computer_use::ObjectKind::File,
            expires_at: "2026-08-29T06:00:00Z".into(),
        };
        with_attachment.draft.attachment_labels = vec!["report.docx".into()];
        with_attachment.attachment = Some(GmailWebAttachmentInput {
            element: upload_element,
            artifact: CreatedFileArtifactOutput {
                file: file.clone(),
                file_name: "report.docx".into(),
                media_type: "application/test".into(),
                size_bytes: 7,
                digest_sha256: digest('a'),
                content: ContentRef::Artifact {
                    artifact_id: file.token,
                    sha256: digest('a'),
                    size_bytes: 7,
                    media_type: "application/test".into(),
                },
            },
        });
        with_attachment.validate().unwrap();
        with_attachment.draft.attachment_labels[0] = "different.docx".into();
        assert_eq!(
            with_attachment.validate(),
            Err(CommunicationContractError::TooManyItems(
                "gmail_web_handoff.attachments"
            ))
        );

        let mut multiple_recipients = input;
        multiple_recipients
            .draft
            .recipients
            .push(LocalDraftRecipient {
                role: RecipientRole::Cc,
                address: "bob@example.com".into(),
                display_name: None,
            });
        assert_eq!(
            multiple_recipients.validate(),
            Err(CommunicationContractError::InvalidRecipientCount)
        );
    }
}
