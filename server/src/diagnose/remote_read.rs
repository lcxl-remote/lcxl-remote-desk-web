//! Edge-side execution of a manager remote read tool call (§8.3).
//!
//! When the agentic loop runs centrally on the manager, the manager ships a
//! server-stamped [`RemoteToolRequest`](desk_agent_protocol::remote_tool::RemoteToolRequest)
//! (one capability call) to this host over the signaling link. The edge keeps
//! **final say** over what may leave the machine: before running anything it
//! re-checks the operation against the envelope's granted scope and the device's
//! local collection policy (the in-process agent itself does not enforce the
//! scope), runs the read, redacts the result fail-closed, and returns a sanitized
//! [`RemoteToolOutput`](desk_agent_protocol::remote_tool::RemoteToolOutput).
//! Raw screenshot bytes are stripped and the model-ready image is carried in a
//! separate field before the router chunks that into a
//! `RemoteToolResponse`. A gate denial or a redaction failure surfaces as an
//! error, never as leaked raw output.

use std::sync::Arc;

use desk_agent_protocol::remote_tool::{RemoteToolImage, RemoteToolOutput};
use desk_agent_protocol::{
    AgentEnvelope, AgentError, AgentErrorKind, AgentOutcome, Capability, DeviceAgent,
};

use super::redaction::{Redactor, redact_snapshot};
use crate::model::settings::SharedSettings;
use crate::worker::agent::LocalDeviceAgent;
use crate::worker::agent::eval::EvidenceSnapshot;

/// Current time as an RFC3339 string.
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// A model-safe permission denial (the edge's final say; the loop turns it into an
/// error tool-result).
fn denied(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::PermissionDenied,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

/// Convert the already-redacted operation result into the only wire shape the
/// manager may receive. Screenshot bytes are fitted first and then erased from
/// the structured outcome; the image travels solely as a validated attachment.
fn sanitize_remote_output(
    cap: Capability,
    mut outcome: AgentOutcome,
) -> Result<RemoteToolOutput, AgentError> {
    let image = if cap == Capability::ScreenCaptureCurrent {
        let AgentOutcome::Ok(desk_agent_protocol::OperationOutput::ReadContext(
            desk_agent_protocol::ReadContextOutput::ScreenCaptureCurrent(shot),
        )) = &mut outcome
        else {
            return Err(AgentError {
                kind: AgentErrorKind::Internal,
                message: "screen capture returned an unexpected output shape".into(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            });
        };
        let fitted = super::model::screenshot::fit_screenshot_to_budget(
            &shot.image,
            super::model::screenshot::DEFAULT_MAX_DIMENSION,
            super::model::screenshot::DEFAULT_MAX_BYTES,
        )
        .map_err(|_| AgentError {
            kind: AgentErrorKind::Internal,
            message: "failed to prepare the screen capture for visual diagnosis".into(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        })?;
        if fitted.jpeg.is_empty() || fitted.jpeg.len() > super::model::screenshot::DEFAULT_MAX_BYTES
        {
            return Err(AgentError {
                kind: AgentErrorKind::InvalidInput,
                message: "screen capture could not be fitted within the image limit".into(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            });
        }
        let image = RemoteToolImage {
            data_url: fitted.to_data_url(),
            media_type: "image/jpeg".into(),
            width: fitted.width,
            height: fitted.height,
            decoded_bytes: fitted.jpeg.len(),
        };
        desk_diagnose_core::image_input::validate_remote_tool_image(&image).map_err(|_| {
            AgentError {
                kind: AgentErrorKind::InvalidInput,
                message: "screen capture failed image validation".into(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            }
        })?;
        shot.image.clear();
        Some(image)
    } else {
        None
    };

    Ok(RemoteToolOutput { outcome, image })
}

/// Runs a single server-stamped read envelope against the in-process device agent
/// and returns the already-redacted outcome (fail-closed). Present only where an
/// in-process worker can read locally (Default / DeskServer), mirroring the
/// diagnose collector's availability.
///
/// The manager stamps the envelope, but the edge keeps **final say**: it re-checks
/// the operation is within the envelope's granted scope and that the device's local
/// collection policy permits it (e.g. logs may be disabled here) before invoking,
/// and redacts fail-closed afterward.
pub struct EdgeReadInvoker {
    agent: Arc<LocalDeviceAgent>,
    redactor: Arc<dyn Redactor>,
    settings: Arc<SharedSettings>,
}

impl EdgeReadInvoker {
    pub fn new(
        agent: Arc<LocalDeviceAgent>,
        redactor: Arc<dyn Redactor>,
        settings: Arc<SharedSettings>,
    ) -> Self {
        Self {
            agent,
            redactor,
            settings,
        }
    }

    /// Invoke `envelope` and return its sanitized [`RemoteToolOutput`]. Re-checks the
    /// edge's gates first (scope consistency + local policy), then invokes; a gate
    /// / exec error or a redaction failure returns `Err` (the router turns it into a
    /// wholesale `RemoteToolResponse::Error`, never leaking raw output). The result
    /// is redacted via the same one-entry snapshot path the Direct read seam uses,
    /// so the exact send-time redaction + screenshot refit run.
    pub async fn invoke_redacted(
        &self,
        envelope: AgentEnvelope,
    ) -> Result<RemoteToolOutput, AgentError> {
        let cap = envelope
            .operation
            .input
            .capability()
            .ok_or_else(|| AgentError {
                kind: AgentErrorKind::UnsupportedCapability,
                message: "remote tool request carries no read capability".to_string(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            })?;

        // Edge re-check 1: the operation must be within the envelope's granted
        // scope. The manager always stamps `granted = [cap]`, so a mismatch is a
        // malformed / forged request — deny rather than run an ungranted operation.
        if !envelope.scope.granted.contains(&cap) {
            return Err(denied("operation is outside the granted scope"));
        }

        // Edge re-check 2: local collection policy has the final say.
        let settings = self.settings.read().await;
        let allow_logs = settings.collection_policy.allow_logs;
        let allow_screen = settings.collection_policy.allow_screen;
        drop(settings);
        let is_log_read = matches!(
            cap,
            Capability::LogRecent | Capability::ContainerLogs | Capability::ContainerInspect
        );
        if is_log_read && !allow_logs {
            return Err(denied("this read is disabled by the device's local policy"));
        }
        if cap == Capability::ScreenCaptureCurrent && !allow_screen {
            return Err(denied(
                "screen capture is disabled by the device's local policy",
            ));
        }

        let output = self.agent.invoke(envelope).await?;

        let mut snapshot = EvidenceSnapshot::record(
            "live",
            String::new(),
            now_rfc3339(),
            vec![(cap, AgentOutcome::Ok(output))],
        );
        redact_snapshot(self.redactor.as_ref(), &mut snapshot).map_err(|e| AgentError {
            kind: AgentErrorKind::RedactionFailed,
            message: format!("evidence redaction failed: {}", e.reason),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        })?;
        let entry = snapshot
            .contexts
            .into_iter()
            .next()
            .expect("the one entry we recorded is present");
        sanitize_remote_output(cap, entry.outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnose::redaction::RegexRedactor;
    use crate::model::settings::Settings;
    use desk_agent_protocol::{
        ActorRef, ActorType, AgentOperation, AgentScope, AuditMeta, CallerRef, CallerType,
        ContextKind, ExecutionMode, LogRecentParams, OperationInput, ProtocolVersion,
        ReadContextInput, RequestId, SystemInfoParams, TargetRef,
    };
    use image::{DynamicImage, ImageBuffer, ImageFormat, Rgb};
    use std::io::Cursor;

    fn settings(allow_logs: bool, allow_screen: bool) -> Arc<SharedSettings> {
        let mut s = Settings::default();
        s.collection_policy.allow_logs = allow_logs;
        s.collection_policy.allow_screen = allow_screen;
        Arc::new(SharedSettings::from(s))
    }

    fn read_envelope(cap_input: ContextKind, granted: Capability) -> AgentEnvelope {
        AgentEnvelope {
            protocol_version: ProtocolVersion::default(),
            request_id: RequestId("rt-1".into()),
            parent_task_id: None,
            target: TargetRef::default(),
            actor: ActorRef {
                actor_type: ActorType::System,
                actor_id: "manager".into(),
            },
            caller: CallerRef {
                caller_type: CallerType::AiModel,
                model_provider: Some("example".into()),
                model_name: Some("m".into()),
                adapter: Some("manager".into()),
            },
            scope: AgentScope {
                granted: vec![granted],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            operation: AgentOperation {
                risk_hint: None,
                input: OperationInput::ReadContext(ReadContextInput { kind: cap_input }),
            },
            audit: AuditMeta {
                approval_id: None,
                reason: Some("remote read".into()),
            },
        }
    }

    /// A granted system-info read runs against the real agent and returns a
    /// redacted `Ok` outcome (succeeds on every CI host).
    #[tokio::test]
    async fn invokes_and_returns_redacted_outcome() {
        let invoker = EdgeReadInvoker::new(
            Arc::new(LocalDeviceAgent::new()),
            Arc::new(RegexRedactor::new()),
            settings(true, false),
        );
        let envelope = read_envelope(
            ContextKind::SystemInfo(SystemInfoParams::default()),
            Capability::SystemInfo,
        );
        let output = invoker.invoke_redacted(envelope).await.expect("read ok");
        let json = serde_json::to_string(&output.outcome).unwrap();
        assert!(json.contains("SystemInfo") || json.contains("hostname"));
        assert!(output.image.is_none());
    }

    /// An envelope whose granted scope does not cover the operation is denied by
    /// the edge's re-check — final say, an `Err`, never raw output.
    #[tokio::test]
    async fn denies_operation_outside_granted_scope() {
        let invoker = EdgeReadInvoker::new(
            Arc::new(LocalDeviceAgent::new()),
            Arc::new(RegexRedactor::new()),
            settings(true, false),
        );
        // Operation reads system info, but the scope grants only process.list.
        let envelope = read_envelope(
            ContextKind::SystemInfo(SystemInfoParams::default()),
            Capability::ProcessList,
        );
        let err = invoker
            .invoke_redacted(envelope)
            .await
            .expect_err("scope mismatch must be denied");
        assert_eq!(err.kind, AgentErrorKind::PermissionDenied);
        assert!(err.safe_for_model);
    }

    /// The device's local policy has final say: a log read is denied when the host
    /// has `allow_logs = false`, even though the manager scope granted it.
    #[tokio::test]
    async fn local_policy_denies_logs_when_disabled() {
        let invoker = EdgeReadInvoker::new(
            Arc::new(LocalDeviceAgent::new()),
            Arc::new(RegexRedactor::new()),
            settings(false, false),
        );
        let envelope = read_envelope(
            ContextKind::LogRecent(LogRecentParams::default()),
            Capability::LogRecent,
        );
        let err = invoker
            .invoke_redacted(envelope)
            .await
            .expect_err("local policy must deny logs");
        assert_eq!(err.kind, AgentErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn local_policy_denies_screen_when_disabled() {
        let invoker = EdgeReadInvoker::new(
            Arc::new(LocalDeviceAgent::new()),
            Arc::new(RegexRedactor::new()),
            settings(true, false),
        );
        let envelope = read_envelope(
            ContextKind::ScreenCaptureCurrent(Default::default()),
            Capability::ScreenCaptureCurrent,
        );
        let err = invoker
            .invoke_redacted(envelope)
            .await
            .expect_err("screen policy must deny before capture");
        assert_eq!(err.kind, AgentErrorKind::PermissionDenied);
        assert!(err.safe_for_model);
    }

    #[test]
    fn sanitized_screen_output_carries_only_the_fitted_attachment() {
        let mut png = Vec::new();
        DynamicImage::ImageRgb8(ImageBuffer::from_pixel(4, 2, Rgb([4, 8, 16])))
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();
        let outcome = AgentOutcome::Ok(desk_agent_protocol::OperationOutput::ReadContext(
            desk_agent_protocol::ReadContextOutput::ScreenCaptureCurrent(
                desk_agent_protocol::ScreenCaptureOutput {
                    format: desk_agent_protocol::ImageFormat::Png,
                    width: 4,
                    height: 2,
                    image: png,
                    truncated: false,
                },
            ),
        ));

        let output = sanitize_remote_output(Capability::ScreenCaptureCurrent, outcome).unwrap();
        let AgentOutcome::Ok(desk_agent_protocol::OperationOutput::ReadContext(
            desk_agent_protocol::ReadContextOutput::ScreenCaptureCurrent(shot),
        )) = &output.outcome
        else {
            panic!("unexpected screen output shape");
        };
        assert!(shot.image.is_empty());
        let image = output.image.expect("fitted image attachment");
        assert!(image.data_url.starts_with("data:image/jpeg;base64,"));
        assert!(image.decoded_bytes <= desk_diagnose_core::image_input::MAX_IMAGE_DECODED_BYTES);
        let json = serde_json::to_string(&output.outcome).unwrap();
        assert!(!json.contains("137,80,78,71"));
    }
}
