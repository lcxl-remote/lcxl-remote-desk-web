//! Pure canonicalization and sealing logic for communication providers.
//!
//! Edge adapters resolve remote identities from the already logged-in target
//! application, but they cannot choose the bytes an exact send grant covers.
//! OSS and Manager use this module to produce and verify the same member and
//! payload digests.

use desk_agent_protocol::communication::{
    COMMUNICATION_SCHEMA_VERSION, CommunicationContractError, CommunicationPayload,
    GmailWebExactSendInput, LocalDraftDocument, RecipientDisplayWarning, RecipientRole,
    ResolvedRecipientMember, SendPayloadSnapshot, SlackWebExactSendInput,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use url::Host;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalEmailAddress {
    pub value: String,
    pub display_warnings: Vec<RecipientDisplayWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommunicationSealError {
    InvalidContract(CommunicationContractError),
    InvalidEmailAddress,
    CanonicalSerialization(String),
    GroupSnapshotDigestMismatch,
    SnapshotDigestMismatch,
}

impl std::fmt::Display for CommunicationSealError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidContract(error) => write!(formatter, "{error}"),
            Self::InvalidEmailAddress => formatter.write_str("email address is invalid"),
            Self::CanonicalSerialization(error) => {
                write!(
                    formatter,
                    "communication canonical serialization failed: {error}"
                )
            }
            Self::GroupSnapshotDigestMismatch => {
                formatter.write_str("group member snapshot digest mismatch")
            }
            Self::SnapshotDigestMismatch => {
                formatter.write_str("send payload snapshot digest mismatch")
            }
        }
    }
}

impl std::error::Error for CommunicationSealError {}

impl From<CommunicationContractError> for CommunicationSealError {
    fn from(value: CommunicationContractError) -> Self {
        Self::InvalidContract(value)
    }
}

/// Canonicalize one mailbox for exact display and comparison. The local part
/// remains case-sensitive; the domain becomes lower-case IDNA ASCII. The
/// adapter-owned stable ID remains the final delivery authority.
pub fn canonicalize_email_address(
    raw: &str,
) -> Result<CanonicalEmailAddress, CommunicationSealError> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.len() > desk_agent_protocol::communication::MAX_ADDRESS_BYTES
        || trimmed.chars().any(|character| {
            character.is_control()
                || character.is_whitespace()
                || is_bidi_or_invisible_control(character)
        })
    {
        return Err(CommunicationSealError::InvalidEmailAddress);
    }
    let Some((local, domain)) = trimmed.rsplit_once('@') else {
        return Err(CommunicationSealError::InvalidEmailAddress);
    };
    if local.is_empty()
        || domain.is_empty()
        || local.contains('@')
        || local.starts_with('.')
        || local.ends_with('.')
        || local.contains("..")
        || local.contains([',', ';', '<', '>', '(', ')', '[', ']'])
    {
        return Err(CommunicationSealError::InvalidEmailAddress);
    }
    let Host::Domain(ascii_domain) =
        Host::parse(domain).map_err(|_| CommunicationSealError::InvalidEmailAddress)?
    else {
        return Err(CommunicationSealError::InvalidEmailAddress);
    };
    if !ascii_domain.contains('.')
        || ascii_domain.starts_with('-')
        || ascii_domain.ends_with('-')
        || ascii_domain.len() > 253
    {
        return Err(CommunicationSealError::InvalidEmailAddress);
    }

    let has_ascii = trimmed
        .chars()
        .any(|character| character.is_ascii_alphanumeric());
    let has_non_ascii = !trimmed.is_ascii();
    let mut display_warnings = Vec::new();
    if has_non_ascii {
        display_warnings.push(RecipientDisplayWarning::UnicodeAddress);
    }
    if has_ascii && has_non_ascii {
        display_warnings.push(RecipientDisplayWarning::MixedAsciiAndNonAscii);
    }
    display_warnings.sort();
    display_warnings.dedup();

    Ok(CanonicalEmailAddress {
        value: format!("{local}@{}", ascii_domain.to_ascii_lowercase()),
        display_warnings,
    })
}

pub fn group_member_snapshot_sha256(
    members: &[ResolvedRecipientMember],
) -> Result<String, CommunicationSealError> {
    if members.is_empty() {
        return Err(CommunicationContractError::MissingGroupSnapshot.into());
    }
    for member in members {
        if member.stable_id.trim().is_empty()
            || member.canonical_address.trim().is_empty()
            || member.stable_id.len()
                > desk_agent_protocol::communication::MAX_COMMUNICATION_ID_BYTES
            || member.canonical_address.len()
                > desk_agent_protocol::communication::MAX_ADDRESS_BYTES
            || member.stable_id.chars().any(char::is_control)
            || member.canonical_address.chars().any(char::is_control)
        {
            return Err(CommunicationContractError::EmptyField("resolved_members").into());
        }
    }
    if members.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(CommunicationContractError::NonCanonicalMembers.into());
    }
    sha256_of(members)
}

pub fn seal_send_payload(
    snapshot_id: String,
    run_id: String,
    payload: CommunicationPayload,
    sealed_at_unix_ms: u64,
) -> Result<SendPayloadSnapshot, CommunicationSealError> {
    payload.validate()?;
    verify_group_snapshots(&payload)?;
    let canonical_payload_sha256 = sha256_of(&SnapshotUnsigned {
        schema_version: COMMUNICATION_SCHEMA_VERSION,
        snapshot_id: &snapshot_id,
        run_id: &run_id,
        payload: &payload,
        sealed_at_unix_ms,
    })?;
    let snapshot = SendPayloadSnapshot {
        schema_version: COMMUNICATION_SCHEMA_VERSION,
        snapshot_id,
        run_id,
        payload,
        sealed_at_unix_ms,
        canonical_payload_sha256,
    };
    snapshot.validate_shape()?;
    Ok(snapshot)
}

pub fn verify_send_payload_snapshot(
    snapshot: &SendPayloadSnapshot,
) -> Result<(), CommunicationSealError> {
    snapshot.validate_shape()?;
    verify_group_snapshots(&snapshot.payload)?;
    let expected = sha256_of(&SnapshotUnsigned {
        schema_version: snapshot.schema_version,
        snapshot_id: &snapshot.snapshot_id,
        run_id: &snapshot.run_id,
        payload: &snapshot.payload,
        sealed_at_unix_ms: snapshot.sealed_at_unix_ms,
    })?;
    if expected != snapshot.canonical_payload_sha256 {
        return Err(CommunicationSealError::SnapshotDigestMismatch);
    }
    Ok(())
}

pub fn send_idempotency_key(
    snapshot: &SendPayloadSnapshot,
) -> Result<String, CommunicationSealError> {
    verify_send_payload_snapshot(snapshot)?;
    Ok(format!("send:v1:{}", snapshot.canonical_payload_sha256))
}

fn body_matches(snapshot: &SendPayloadSnapshot, body: &str) -> bool {
    snapshot.payload.body.size_bytes == body.len() as u64
        && snapshot.payload.body.digest_sha256 == format!("{:x}", Sha256::digest(body.as_bytes()))
}

pub fn verify_gmail_web_exact_send_input(
    input: &GmailWebExactSendInput,
) -> Result<(), CommunicationSealError> {
    input.validate_shape()?;
    let snapshot = input
        .handoff
        .send_payload_snapshot
        .as_ref()
        .ok_or(CommunicationContractError::InvalidSendAuthority)?;
    verify_send_payload_snapshot(snapshot)?;
    let canonical = canonicalize_email_address(&input.draft.recipients[0].address)?;
    let expected_attachments = input
        .draft
        .attachment_labels
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let actual_attachments = snapshot
        .payload
        .attachments
        .iter()
        .map(|attachment| attachment.file_name.as_str())
        .collect::<Vec<_>>();
    if snapshot.payload.recipients.len() != 1
        || snapshot.payload.recipients[0].role != RecipientRole::To
        || snapshot.payload.recipients[0].canonical_address != canonical.value
        || snapshot.payload.recipients[0].display_name != input.draft.recipients[0].display_name
        || snapshot.payload.recipients[0].display_warnings != canonical.display_warnings
        || snapshot.payload.subject != input.draft.subject
        || !body_matches(snapshot, &input.draft.body_plain_text)
        || actual_attachments != expected_attachments
    {
        return Err(CommunicationSealError::SnapshotDigestMismatch);
    }
    Ok(())
}

pub fn verify_slack_web_exact_send_input(
    input: &SlackWebExactSendInput,
) -> Result<(), CommunicationSealError> {
    input.validate_shape()?;
    let snapshot = input
        .handoff
        .send_payload_snapshot
        .as_ref()
        .ok_or(CommunicationContractError::InvalidSendAuthority)?;
    verify_send_payload_snapshot(snapshot)?;
    if snapshot.payload.recipients.len() != 1
        || snapshot.payload.recipients[0].role != RecipientRole::ChatDestination
        || snapshot.payload.recipients[0].canonical_address != input.composer.accessible_name.trim()
        || !snapshot.payload.subject.is_empty()
        || !snapshot.payload.attachments.is_empty()
        || !body_matches(snapshot, &input.body_plain_text)
    {
        return Err(CommunicationSealError::SnapshotDigestMismatch);
    }
    Ok(())
}

/// Render a local-only draft as inert UTF-8 text. This intentionally does not
/// emit HTML, Markdown, MIME, clickable links, remote images, or executable
/// fields. A later edge adapter must resolve recipients and build a new
/// immutable payload rather than treating these bytes as send authority.
pub fn render_local_draft_text(
    draft: &LocalDraftDocument,
) -> Result<Vec<u8>, CommunicationSealError> {
    draft.validate()?;
    let mut output = String::from("LCXL LOCAL COMMUNICATION DRAFT\n");
    output.push_str("UNVERIFIED RECIPIENT INTENT - NOT SENT\n\n");
    for role in [
        RecipientRole::To,
        RecipientRole::Cc,
        RecipientRole::Bcc,
        RecipientRole::ChatDestination,
    ] {
        let values = draft
            .recipients
            .iter()
            .filter(|recipient| recipient.role == role)
            .map(|recipient| match &recipient.display_name {
                Some(name) => format!("{name} <{}>", recipient.address),
                None => recipient.address.clone(),
            })
            .collect::<Vec<_>>();
        if !values.is_empty() {
            output.push_str(match role {
                RecipientRole::To => "To: ",
                RecipientRole::Cc => "Cc: ",
                RecipientRole::Bcc => "Bcc: ",
                RecipientRole::ChatDestination => "Chat destination: ",
            });
            output.push_str(&values.join(", "));
            output.push('\n');
        }
    }
    output.push_str("Subject: ");
    output.push_str(&draft.subject);
    output.push_str("\n\n");
    output.push_str(&draft.body_plain_text);
    if !output.ends_with('\n') {
        output.push('\n');
    }
    if !draft.attachment_labels.is_empty() {
        output.push_str("\nAttachment references (not embedded):\n");
        for label in &draft.attachment_labels {
            output.push_str("- ");
            output.push_str(label);
            output.push('\n');
        }
    }
    Ok(output.into_bytes())
}

#[derive(Serialize)]
struct SnapshotUnsigned<'a> {
    schema_version: u16,
    snapshot_id: &'a str,
    run_id: &'a str,
    payload: &'a CommunicationPayload,
    sealed_at_unix_ms: u64,
}

fn verify_group_snapshots(payload: &CommunicationPayload) -> Result<(), CommunicationSealError> {
    for recipient in &payload.recipients {
        if let Some(actual) = &recipient.member_snapshot_sha256 {
            let expected = group_member_snapshot_sha256(&recipient.resolved_members)?;
            if &expected != actual {
                return Err(CommunicationSealError::GroupSnapshotDigestMismatch);
            }
        }
    }
    Ok(())
}

fn sha256_of(value: &(impl Serialize + ?Sized)) -> Result<String, CommunicationSealError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| CommunicationSealError::CanonicalSerialization(error.to_string()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn is_bidi_or_invisible_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200b}'
            | '\u{200c}'
            | '\u{200d}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2069}'
            | '\u{feff}'
    )
}

#[cfg(test)]
pub(crate) mod test_support {
    use desk_agent_protocol::{
        browser_control::{
            BROWSER_CONTROL_SCHEMA_VERSION, BrowserAdapterRef, BrowserElementRef,
            BrowserElementRole, BrowserEngineKind, BrowserOrigin, BrowserOriginKind,
            BrowserPageRef,
        },
        communication::{
            COMMUNICATION_SCHEMA_VERSION, CommunicationChannel, CommunicationDraftHandoff,
            CommunicationPayload, CommunicationPrepareVerification, CommunicationSendAuthority,
            CommunicationSurfaceKind, CommunicationSurfaceRef, CommunicationSurfaceScope,
            GmailWebExactSendInput, ImmutableBodySnapshot, LocalDraftDocument, LocalDraftRecipient,
            RecipientIdentity, RecipientKind, RecipientRole, SlackWebExactSendInput,
        },
        data_lineage::ContentRef,
    };
    use sha2::{Digest, Sha256};

    use super::seal_send_payload;

    fn page(host: &str) -> BrowserPageRef {
        BrowserPageRef {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            adapter: BrowserAdapterRef {
                engine: BrowserEngineKind::ChromeExtension,
                device_id: "device".into(),
                os_session_id: "session".into(),
                browser_major_version: 151,
                browser_version: "151.0".into(),
                adapter_id: "chrome-extension".into(),
                adapter_version: "1".into(),
                profile_incarnation: "profile".into(),
                connection_revision: 7,
            },
            page_id: "page".into(),
            page_incarnation: "page-incarnation".into(),
            origin: BrowserOrigin {
                kind: BrowserOriginKind::Https,
                host_ascii: host.into(),
                port: 443,
            },
            document_revision: 3,
            url_sha256: "a".repeat(64),
            observed_at_unix_ms: 200,
        }
    }

    fn element(
        page: &BrowserPageRef,
        element_id: &str,
        role: BrowserElementRole,
        accessible_name: &str,
    ) -> BrowserElementRef {
        BrowserElementRef {
            page_id: page.page_id.clone(),
            page_incarnation: page.page_incarnation.clone(),
            document_revision: page.document_revision,
            element_id: element_id.into(),
            role,
            accessible_name: accessible_name.into(),
            value: None,
            element_revision: 1,
        }
    }

    fn body(value: &str) -> ImmutableBodySnapshot {
        let digest = format!("{:x}", Sha256::digest(value.as_bytes()));
        ImmutableBodySnapshot {
            content: ContentRef::ImmutableBlob {
                blob_id: format!("body-{digest}"),
                sha256: digest.clone(),
                size_bytes: value.len() as u64,
                media_type: "text/plain; charset=utf-8".into(),
            },
            media_type: "text/plain; charset=utf-8".into(),
            size_bytes: value.len() as u64,
            digest_sha256: digest,
        }
    }

    fn handoff(payload: CommunicationPayload, kind: &str) -> CommunicationDraftHandoff {
        let surface = payload.surface.clone();
        let snapshot =
            seal_send_payload(format!("{kind}-send-snapshot"), "run".into(), payload, 100).unwrap();
        CommunicationDraftHandoff {
            schema_version: COMMUNICATION_SCHEMA_VERSION,
            handoff_id: format!("{kind}-handoff"),
            run_id: "run".into(),
            surface,
            compose_id: format!("{kind}-compose"),
            prepared_payload_sha256: "b".repeat(64),
            verification: CommunicationPrepareVerification::SemanticExact,
            readback_payload_sha256: Some("b".repeat(64)),
            send_authority: CommunicationSendAuthority::ExactGrantEligible,
            send_payload_snapshot: Some(snapshot),
            handed_off_at_unix_ms: 100,
        }
    }

    pub(crate) fn gmail_exact_send_input() -> GmailWebExactSendInput {
        let page = page("mail.google.com");
        let draft = LocalDraftDocument {
            schema_version: COMMUNICATION_SCHEMA_VERSION,
            recipients: vec![LocalDraftRecipient {
                role: RecipientRole::To,
                address: "alice@example.com".into(),
                display_name: Some("Alice".into()),
            }],
            subject: "Reviewed subject".into(),
            body_plain_text: "Reviewed Gmail body".into(),
            attachment_labels: Vec::new(),
        };
        let surface = CommunicationSurfaceRef {
            channel: CommunicationChannel::Email,
            kind: CommunicationSurfaceKind::ChromeExtension,
            scope: CommunicationSurfaceScope::WebOrigin {
                origin: page.origin.clone(),
            },
            device_id: page.adapter.device_id.clone(),
            os_session_id: page.adapter.os_session_id.clone(),
            adapter_id: "gmail-web".into(),
            adapter_version: "1".into(),
            profile_id: page.adapter.profile_incarnation.clone(),
            account_id: "gmail-current-profile".into(),
            revision: page.adapter.connection_revision,
        };
        let handoff = handoff(
            CommunicationPayload {
                surface,
                recipients: vec![RecipientIdentity {
                    role: RecipientRole::To,
                    kind: RecipientKind::EmailMailbox,
                    stable_id: "gmail-mailbox-alice".into(),
                    canonical_address: "alice@example.com".into(),
                    display_name: Some("Alice".into()),
                    display_warnings: Vec::new(),
                    resolved_members: Vec::new(),
                    member_snapshot_sha256: None,
                }],
                subject: draft.subject.clone(),
                body: body(&draft.body_plain_text),
                attachments: Vec::new(),
            },
            "gmail",
        );
        GmailWebExactSendInput {
            schema_version: COMMUNICATION_SCHEMA_VERSION,
            handoff,
            to_field: element(&page, "to", BrowserElementRole::Combobox, "To recipients"),
            subject_field: element(&page, "subject", BrowserElementRole::Textbox, "Subject"),
            body_field: element(&page, "body", BrowserElementRole::Textbox, "Message Body"),
            send_control: element(&page, "send", BrowserElementRole::Button, "Send"),
            page,
            draft,
        }
    }

    pub(crate) fn slack_exact_send_input() -> SlackWebExactSendInput {
        let page = page("app.slack.com");
        let body_plain_text = "Reviewed Slack body".to_string();
        let composer = element(
            &page,
            "composer",
            BrowserElementRole::Textbox,
            "Message #review",
        );
        let surface = CommunicationSurfaceRef {
            channel: CommunicationChannel::Chat,
            kind: CommunicationSurfaceKind::ChromeExtension,
            scope: CommunicationSurfaceScope::WebOrigin {
                origin: page.origin.clone(),
            },
            device_id: page.adapter.device_id.clone(),
            os_session_id: page.adapter.os_session_id.clone(),
            adapter_id: "slack-web".into(),
            adapter_version: "1".into(),
            profile_id: page.adapter.profile_incarnation.clone(),
            account_id: "slack-current-profile".into(),
            revision: page.adapter.connection_revision,
        };
        let handoff = handoff(
            CommunicationPayload {
                surface,
                recipients: vec![RecipientIdentity {
                    role: RecipientRole::ChatDestination,
                    kind: RecipientKind::ChatChannel,
                    stable_id: "slack-channel-review".into(),
                    canonical_address: composer.accessible_name.clone(),
                    display_name: None,
                    display_warnings: Vec::new(),
                    resolved_members: Vec::new(),
                    member_snapshot_sha256: None,
                }],
                subject: String::new(),
                body: body(&body_plain_text),
                attachments: Vec::new(),
            },
            "slack",
        );
        SlackWebExactSendInput {
            schema_version: COMMUNICATION_SCHEMA_VERSION,
            handoff,
            composer,
            send_control: element(&page, "send", BrowserElementRole::Button, "Send message"),
            page,
            body_plain_text,
        }
    }
}

#[cfg(test)]
mod tests {
    use desk_agent_protocol::{
        browser_control::{BrowserOrigin, BrowserOriginKind},
        communication::{
            CommunicationChannel, CommunicationSurfaceKind, CommunicationSurfaceRef,
            CommunicationSurfaceScope, ImmutableBodySnapshot, LocalDraftDocument,
            LocalDraftRecipient, RecipientIdentity, RecipientKind, RecipientRole,
        },
        data_lineage::ContentRef,
    };

    use super::*;

    fn digest(byte: char) -> String {
        byte.to_string().repeat(64)
    }

    fn payload() -> CommunicationPayload {
        CommunicationPayload {
            surface: CommunicationSurfaceRef {
                channel: CommunicationChannel::Email,
                kind: CommunicationSurfaceKind::ChromeDevtoolsMcp,
                scope: CommunicationSurfaceScope::WebOrigin {
                    origin: BrowserOrigin {
                        kind: BrowserOriginKind::Https,
                        host_ascii: "mail.google.com".into(),
                        port: 443,
                    },
                },
                device_id: "device-1".into(),
                os_session_id: "session-1".into(),
                adapter_id: "gmail-web-v1".into(),
                adapter_version: "1.0.0".into(),
                profile_id: "browser-profile-1".into(),
                account_id: "account-1".into(),
                revision: 2,
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
            subject: "Report".into(),
            body: ImmutableBodySnapshot {
                content: ContentRef::ImmutableBlob {
                    blob_id: "body-1".into(),
                    sha256: digest('a'),
                    size_bytes: 6,
                    media_type: "text/plain".into(),
                },
                media_type: "text/plain".into(),
                size_bytes: 6,
                digest_sha256: digest('a'),
            },
            attachments: Vec::new(),
        }
    }

    #[test]
    fn email_domain_is_idna_canonical_and_unicode_is_visible() {
        let canonical = canonicalize_email_address("User@BÜCHER.example").unwrap();
        assert_eq!(canonical.value, "User@xn--bcher-kva.example");
        assert_eq!(
            canonical.display_warnings,
            vec![
                RecipientDisplayWarning::UnicodeAddress,
                RecipientDisplayWarning::MixedAsciiAndNonAscii,
            ]
        );
        assert!(canonicalize_email_address("a@example.com\u{202e}").is_err());
    }

    #[test]
    fn snapshot_digest_binds_final_recipient_and_body() {
        let snapshot =
            seal_send_payload("snapshot-1".into(), "run-1".into(), payload(), 42).unwrap();
        verify_send_payload_snapshot(&snapshot).unwrap();
        assert_eq!(
            send_idempotency_key(&snapshot).unwrap(),
            format!("send:v1:{}", snapshot.canonical_payload_sha256)
        );

        let mut changed = snapshot;
        changed.payload.recipients[0].canonical_address = "mallory@example.com".into();
        assert_eq!(
            verify_send_payload_snapshot(&changed),
            Err(CommunicationSealError::SnapshotDigestMismatch)
        );
    }

    #[test]
    fn group_digest_binds_stable_member_snapshot() {
        let members = vec![
            ResolvedRecipientMember {
                stable_id: "user:a".into(),
                canonical_address: "a".into(),
            },
            ResolvedRecipientMember {
                stable_id: "user:b".into(),
                canonical_address: "b".into(),
            },
        ];
        let digest = group_member_snapshot_sha256(&members).unwrap();
        let mut changed = members;
        changed[1].canonical_address = "c".into();
        assert_ne!(group_member_snapshot_sha256(&changed).unwrap(), digest);
    }

    #[test]
    fn local_draft_is_inert_and_explicitly_unsent() {
        let bytes = render_local_draft_text(&LocalDraftDocument {
            schema_version: COMMUNICATION_SCHEMA_VERSION,
            recipients: vec![LocalDraftRecipient {
                role: RecipientRole::To,
                address: "alice@example.com".into(),
                display_name: Some("Alice".into()),
            }],
            subject: "Report".into(),
            body_plain_text: "Review https://example.com before sending.".into(),
            attachment_labels: vec!["report.docx".into()],
        })
        .unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("UNVERIFIED RECIPIENT INTENT - NOT SENT"));
        assert!(text.contains("To: Alice <alice@example.com>"));
        assert!(!text.contains("<html"));
        assert!(!text.contains("Content-Type:"));
    }
}
