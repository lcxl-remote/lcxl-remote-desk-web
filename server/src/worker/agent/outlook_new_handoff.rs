//! Windows Outlook (new) compose handoff.
//!
//! This adapter deliberately stops at a visible `mailto:` compose surface. It
//! never accepts an executable path from the caller, never attaches files, and
//! never clicks Send. `ShellExecuteW` success only proves that Windows accepted
//! the registered protocol launch; it is reported as assistive/unverified.

use desk_agent_protocol::communication::{
    COMMUNICATION_SCHEMA_VERSION, CommunicationDraftHandoff, CommunicationPrepareVerification,
    CommunicationSendAuthority, OutlookNewComposeHandoffRequest, RecipientRole,
};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use sha2::{Digest, Sha256};

pub use desk_diagnose_core::device_assistant::OUTLOOK_NEW_APPLICATION_ID;
const MAX_MAILTO_URI_BYTES: usize = 24 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlookNewHandler {
    pub executable_path: String,
}

#[cfg(windows)]
pub fn probe_handler() -> Result<OutlookNewHandler, AgentError> {
    use windows::Win32::UI::Shell::{ASSOCF_NONE, ASSOCSTR_EXECUTABLE, AssocQueryStringW};
    use windows::core::{PCWSTR, PWSTR};

    let association = "mailto\0".encode_utf16().collect::<Vec<_>>();
    let mut output = vec![0u16; 32_768];
    let mut output_len = u32::try_from(output.len()).expect("fixed buffer length fits u32");
    // SAFETY: both UTF-16 buffers are NUL-terminated/writable for the supplied
    // lengths, and remain alive for the duration of the call.
    let status = unsafe {
        AssocQueryStringW(
            ASSOCF_NONE,
            ASSOCSTR_EXECUTABLE,
            PCWSTR(association.as_ptr()),
            PCWSTR::null(),
            Some(PWSTR(output.as_mut_ptr())),
            &mut output_len,
        )
    };
    if status.is_ok() {
        let used = output
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(output.len());
        let executable_path = String::from_utf16(&output[..used])
            .map_err(|_| adapter_error("the registered mailto handler path is not valid UTF-16"))?;
        if is_outlook_new_executable(&executable_path) {
            return Ok(OutlookNewHandler { executable_path });
        }
    }

    // Outlook (new) can be registered as a packaged AppExecutionAlias even
    // when Windows has no default `mailto:` UserChoice. The alias is used only
    // as a readiness proof; execution below uses the fixed AppUserModelID.
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let local_app_data = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| adapter_error("LOCALAPPDATA is unavailable in the interactive session"))?;
    let alias = std::path::PathBuf::from(local_app_data)
        .join("Microsoft")
        .join("WindowsApps")
        .join("olk.exe");
    let metadata = std::fs::symlink_metadata(&alias)
        .map_err(|_| adapter_error("the Outlook (new) app execution alias is unavailable"))?;
    if !metadata.is_file()
        || metadata.len() != 0
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
    {
        return Err(adapter_error(
            "the Outlook (new) app execution alias is not a reviewed package alias",
        ));
    }
    let executable_path = alias.to_string_lossy().into_owned();
    if !is_outlook_new_executable(&executable_path) {
        return Err(adapter_error(
            "the Outlook (new) app execution alias has an unexpected identity",
        ));
    }
    Ok(OutlookNewHandler { executable_path })
}

#[cfg(not(windows))]
pub fn probe_handler() -> Result<OutlookNewHandler, AgentError> {
    Err(adapter_error(
        "Outlook (new) compose handoff is supported only on Windows",
    ))
}

#[cfg(windows)]
fn is_outlook_new_executable(path: &str) -> bool {
    let normalized = path.replace('/', "\\").to_ascii_lowercase();
    (normalized.ends_with("\\olk.exe")
        && normalized.contains("\\windowsapps\\microsoft.outlookforwindows_")
        && normalized.contains("__8wekyb3d8bbwe\\olk.exe"))
        || normalized.ends_with("\\appdata\\local\\microsoft\\windowsapps\\olk.exe")
}

#[cfg(not(windows))]
fn is_outlook_new_executable(_path: &str) -> bool {
    false
}

pub fn preflight(
    request: &OutlookNewComposeHandoffRequest,
) -> Result<OutlookNewHandler, AgentError> {
    request
        .validate()
        .map_err(|error| invalid_input(format!("invalid Outlook handoff request: {error}")))?;
    if request.surface.scope
        != (desk_agent_protocol::communication::CommunicationSurfaceScope::DesktopApplication {
            application_id: OUTLOOK_NEW_APPLICATION_ID.into(),
        })
    {
        return Err(invalid_input(
            "Outlook handoff surface is not bound to the reviewed application identity",
        ));
    }
    for recipient in &request.draft.recipients {
        desk_diagnose_core::communication::canonicalize_email_address(&recipient.address)
            .map_err(|_| invalid_input("Outlook handoff contains an invalid email address"))?;
    }
    let uri = build_mailto_uri(request)?;
    if uri.len() > MAX_MAILTO_URI_BYTES {
        return Err(invalid_input(
            "Outlook handoff fields exceed the bounded mailto launch budget",
        ));
    }
    probe_handler()
}

pub fn execute(
    request: &OutlookNewComposeHandoffRequest,
) -> Result<CommunicationDraftHandoff, AgentError> {
    let _handler = preflight(request)?;
    let uri = build_mailto_uri(request)?;
    launch_mailto(&uri)?;
    let prepared_payload_sha256 = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(request).map_err(|error| {
            adapter_error(format!("failed to seal Outlook handoff input: {error}"))
        })?)
    );
    let handed_off_at_unix_ms = u64::try_from(chrono::Utc::now().timestamp_millis())
        .map_err(|_| adapter_error("system clock predates the Unix epoch"))?;
    let handoff = CommunicationDraftHandoff {
        schema_version: COMMUNICATION_SCHEMA_VERSION,
        handoff_id: format!("outlook-new-handoff-{}", request.call_id),
        run_id: request.run_id.clone(),
        surface: request.surface.clone(),
        compose_id: format!("outlook-new-mailto-{}", request.call_id),
        prepared_payload_sha256,
        verification: CommunicationPrepareVerification::AssistiveUnverified,
        readback_payload_sha256: None,
        send_authority: CommunicationSendAuthority::ManualOnly,
        handed_off_at_unix_ms,
    };
    handoff
        .validate()
        .map_err(|error| adapter_error(format!("invalid Outlook handoff result: {error}")))?;
    Ok(handoff)
}

fn build_mailto_uri(request: &OutlookNewComposeHandoffRequest) -> Result<String, AgentError> {
    let addresses = |role| {
        request
            .draft
            .recipients
            .iter()
            .filter(|recipient| recipient.role == role)
            .map(|recipient| {
                desk_diagnose_core::communication::canonicalize_email_address(&recipient.address)
                    .map(|address| address.value)
                    .map_err(|_| invalid_input("Outlook handoff contains an invalid email address"))
            })
            .collect::<Result<Vec<_>, _>>()
    };
    let to = addresses(RecipientRole::To)?;
    let cc = addresses(RecipientRole::Cc)?;
    let bcc = addresses(RecipientRole::Bcc)?;
    let mut uri = url::Url::parse(&format!("mailto:{}", to.join(",")))
        .map_err(|_| invalid_input("failed to construct the Outlook mailto target"))?;
    {
        let mut query = uri.query_pairs_mut();
        if !cc.is_empty() {
            query.append_pair("cc", &cc.join(","));
        }
        if !bcc.is_empty() {
            query.append_pair("bcc", &bcc.join(","));
        }
        query.append_pair("subject", &request.draft.subject);
        query.append_pair("body", &request.draft.body_plain_text);
    }
    Ok(uri.into())
}

#[cfg(windows)]
fn launch_mailto(uri: &str) -> Result<(), AgentError> {
    use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize,
    };
    use windows::Win32::UI::Shell::{
        AO_NONE, ApplicationActivationManager, IApplicationActivationManager,
    };
    use windows::core::PCWSTR;

    let application_id = OUTLOOK_NEW_APPLICATION_ID
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let uri = uri
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: COM receives fixed class/application identities and one bounded,
    // NUL-terminated URI argument. No executable path or shell command is
    // caller-controlled.
    unsafe {
        let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let uninitialize = initialized.is_ok();
        if initialized.is_err() && initialized != RPC_E_CHANGED_MODE {
            return Err(adapter_error(format!(
                "failed to initialize Windows application activation: {}",
                initialized.message()
            )));
        }
        let activation: IApplicationActivationManager =
            CoCreateInstance(&ApplicationActivationManager, None, CLSCTX_INPROC_SERVER).map_err(
                |error| {
                    if uninitialize {
                        CoUninitialize();
                    }
                    adapter_error(format!(
                        "failed to create Windows application activator: {error}"
                    ))
                },
            )?;
        let result = activation.ActivateApplication(
            PCWSTR(application_id.as_ptr()),
            PCWSTR(uri.as_ptr()),
            AO_NONE,
        );
        if uninitialize {
            CoUninitialize();
        }
        result.map_err(|error| {
            adapter_error(format!(
                "Windows rejected the Outlook compose activation: {error}"
            ))
        })?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn launch_mailto(_uri: &str) -> Result<(), AgentError> {
    Err(adapter_error(
        "Outlook (new) compose handoff is supported only on Windows",
    ))
}

fn invalid_input(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

fn adapter_error(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::UnsupportedCapability,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::communication::{
        CommunicationChannel, CommunicationSurfaceKind, CommunicationSurfaceRef,
        CommunicationSurfaceScope, LocalDraftDocument, LocalDraftRecipient,
    };

    fn request() -> OutlookNewComposeHandoffRequest {
        OutlookNewComposeHandoffRequest {
            schema_version: COMMUNICATION_SCHEMA_VERSION,
            call_id: "call-1".into(),
            run_id: "run-1".into(),
            surface: CommunicationSurfaceRef {
                channel: CommunicationChannel::Email,
                kind: CommunicationSurfaceKind::OutlookNewDesktop,
                scope: CommunicationSurfaceScope::DesktopApplication {
                    application_id: OUTLOOK_NEW_APPLICATION_ID.into(),
                },
                device_id: "device-1".into(),
                os_session_id: "session-1".into(),
                adapter_id: "communication.outlook_new.mailto.edge".into(),
                adapter_version: "outlook-new-mailto-handoff/v1".into(),
                profile_id: "session-1".into(),
                account_id: "unverified-current-account".into(),
                revision: 1,
            },
            draft: LocalDraftDocument {
                schema_version: COMMUNICATION_SCHEMA_VERSION,
                recipients: vec![LocalDraftRecipient {
                    role: RecipientRole::To,
                    address: "Person@Example.COM".into(),
                    display_name: None,
                }],
                subject: "Quarterly review & next steps".into(),
                body_plain_text: "Line one\nLine two".into(),
                attachment_labels: vec![],
            },
        }
    }

    #[test]
    fn mailto_builder_canonicalizes_and_encodes_fields() {
        let uri = build_mailto_uri(&request()).unwrap();
        assert!(uri.starts_with("mailto:Person@example.com?"), "{uri}");
        assert!(uri.contains("subject=Quarterly+review+%26+next+steps"));
        assert!(uri.contains("body=Line+one%0ALine+two"));
        assert!(!uri.contains("display_name"));
    }

    #[test]
    fn attachment_and_chat_inputs_fail_closed() {
        let mut input = request();
        input.draft.attachment_labels.push("report.docx".into());
        assert!(input.validate().is_err());
        input.draft.attachment_labels.clear();
        input.draft.recipients[0].role = RecipientRole::ChatDestination;
        assert!(input.validate().is_err());
    }

    #[test]
    fn only_reviewed_windowsapps_outlook_binary_matches() {
        assert!(is_outlook_new_executable(
            r"C:\Program Files\WindowsApps\Microsoft.OutlookForWindows_1.2025.312.200_x64__8wekyb3d8bbwe\olk.exe"
        ));
        assert!(!is_outlook_new_executable(r"C:\Temp\olk.exe"));
        assert!(!is_outlook_new_executable(
            r"C:\Program Files\Microsoft Office\root\Office16\OUTLOOK.EXE"
        ));
        assert!(is_outlook_new_executable(
            r"C:\Users\dev\AppData\Local\Microsoft\WindowsApps\olk.exe"
        ));
    }

    #[test]
    #[ignore = "requires Outlook (new) to be the current Windows mailto handler"]
    fn live_outlook_new_handler_probe() {
        let handler = probe_handler().expect("reviewed Outlook (new) mailto handler");
        assert!(is_outlook_new_executable(&handler.executable_path));
    }

    #[test]
    #[ignore = "opens one visible Outlook (new) compose handoff and never sends"]
    fn live_outlook_new_manual_handoff() {
        assert_eq!(
            std::env::var("LCXL_LIVE_OUTLOOK_HANDOFF").as_deref(),
            Ok("1"),
            "set LCXL_LIVE_OUTLOOK_HANDOFF=1 to acknowledge the cloud-draft side effect"
        );
        let mut input = request();
        input.draft.recipients[0].address = "review@example.invalid".into();
        input.draft.subject = "[LCXL test] Manual handoff - do not send".into();
        input.draft.body_plain_text =
            "This is a local development validation of Outlook compose handoff. Do not send."
                .into();
        let handoff = execute(&input).expect("Outlook manual handoff");
        assert_eq!(
            handoff.verification,
            CommunicationPrepareVerification::AssistiveUnverified
        );
        assert_eq!(
            handoff.send_authority,
            CommunicationSendAuthority::ManualOnly
        );
        assert!(handoff.readback_payload_sha256.is_none());
    }
}
