//! Command streams remain distinct from the bounded execution receipt.
use super::batch::{
    DeliveredPart, DeliveryIdentity, OutputPart, PartContent, PreparedAttachment, prepare_delivery,
};
use desk_agent_protocol::{AgentError, ExecOutput, ExecOutputStreams};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandReceipt {
    pub started: bool,
    pub exit_code: Option<i32>,
    pub termination_signal: Option<i32>,
    pub failure: Option<AgentError>,
    pub diagnostics: Vec<desk_agent_protocol::native_diagnostic::NativeDiagnostic>,
    pub duration_ms: u32,
    pub redactions_applied: bool,
    pub streams: Vec<DeliveredPart>,
}

#[derive(Debug)]
pub struct PreparedCommand {
    pub receipt: CommandReceipt,
    pub attachments: Vec<PreparedAttachment>,
}

/// Call only for a completed command with a typed ExecOutput. Timeout/unknown
/// outcomes retain their own state instead of being invented from stream text.
pub fn prepare_command(
    identity: &DeliveryIdentity<'_>,
    output: ExecOutput,
    created_at_unix_ms: u64,
) -> Result<PreparedCommand, AgentError> {
    let parts = match output.streams {
        ExecOutputStreams::Split {
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        } => vec![
            OutputPart {
                name: "stdout".into(),
                content: PartContent::Text(stdout),
                source_truncated: stdout_truncated,
            },
            OutputPart {
                name: "stderr".into(),
                content: PartContent::Text(stderr),
                source_truncated: stderr_truncated,
            },
        ],
        ExecOutputStreams::PtyCombined {
            terminal,
            truncated,
        } => vec![OutputPart {
            name: "terminal".into(),
            content: PartContent::Text(terminal),
            source_truncated: truncated,
        }],
    };
    let delivery = prepare_delivery(identity, parts, created_at_unix_ms)?;
    Ok(PreparedCommand {
        receipt: CommandReceipt {
            started: output.started,
            exit_code: output.exit_code,
            termination_signal: output.termination_signal,
            failure: output.failure,
            diagnostics: output.diagnostics,
            duration_ms: output.duration_ms,
            redactions_applied: !output.redactions.is_empty(),
            streams: delivery.parts,
        },
        attachments: delivery.attachments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity() -> DeliveryIdentity<'static> {
        DeliveryIdentity {
            conversation_id: "conversation",
            actor_id: "owner",
            device_id: "device",
            message_id: "message",
            tool_call_id: "call",
        }
    }
    #[test]
    fn failed_command_keeps_diagnostics_when_streams_are_externalized() {
        let diagnostic = desk_agent_protocol::native_diagnostic::NativeDiagnostic::from_io(
            desk_agent_protocol::native_diagnostic::DiagnosticStage::OutputRead,
            "stderr",
            &std::io::Error::from_raw_os_error(5),
        );
        let prepared = prepare_command(
            &identity(),
            ExecOutput {
                started: true,
                exit_code: None,
                termination_signal: None,
                failure: Some(AgentError {
                    kind: desk_agent_protocol::AgentErrorKind::Timeout,
                    message: "Timed out".into(),
                    retryable: false,
                    safe_for_model: true,
                    error_code: None,
                }),
                diagnostics: vec![diagnostic.clone()],
                streams: ExecOutputStreams::Split {
                    stdout: "retained".repeat(1000),
                    stderr: "error".repeat(1000),
                    stdout_truncated: false,
                    stderr_truncated: false,
                },
                duration_ms: 100,
                redactions: vec![],
            },
            1,
        )
        .unwrap();
        assert_eq!(prepared.attachments.len(), 2);
        assert_eq!(prepared.receipt.exit_code, None);
        assert_eq!(prepared.receipt.diagnostics, vec![diagnostic]);
        assert_eq!(
            prepared.receipt.failure.unwrap().kind,
            desk_agent_protocol::AgentErrorKind::Timeout
        );
        assert_eq!(prepared.attachments[0].content, b"retained".repeat(1000));
        assert_eq!(prepared.attachments[1].content, b"error".repeat(1000));
    }
    #[test]
    fn typed_pty_result_does_not_invent_split_streams() {
        let prepared = prepare_command(
            &identity(),
            ExecOutput {
                started: true,
                termination_signal: None,
                failure: None,
                diagnostics: vec![],
                exit_code: Some(17),
                duration_ms: 23,
                redactions: vec!["secret".into()],
                streams: ExecOutputStreams::PtyCombined {
                    terminal: "x".repeat(5000),
                    truncated: true,
                },
            },
            1,
        )
        .unwrap();
        assert_eq!(prepared.receipt.exit_code, Some(17));
        assert_eq!(prepared.receipt.duration_ms, 23);
        assert!(prepared.receipt.redactions_applied);
        assert_eq!(prepared.receipt.streams.len(), 1);
        assert_eq!(prepared.receipt.streams[0].name, "terminal");
        assert!(prepared.receipt.streams[0].source_truncated);
        assert_eq!(prepared.attachments.len(), 1);
        let serialized = serde_json::to_string(&prepared.receipt).unwrap();
        assert!(!serialized.contains("stdout"));
        assert!(!serialized.contains("stderr"));
        assert!(!serialized.contains("secret"));
    }
    #[test]
    fn empty_stream_has_no_attachment_and_large_stream_retains_storage_limit() {
        let prepared = prepare_command(
            &identity(),
            ExecOutput {
                started: true,
                termination_signal: None,
                failure: None,
                diagnostics: vec![],
                exit_code: Some(0),
                duration_ms: 2,
                redactions: vec![],
                streams: ExecOutputStreams::Split {
                    stdout: "x".repeat(400001),
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                },
            },
            1,
        )
        .unwrap();
        assert_eq!(prepared.attachments.len(), 1);
        assert_eq!(prepared.attachments[0].content.len(), 400000);
        assert!(prepared.attachments[0].metadata.storage_truncated);
        assert!(!prepared.receipt.streams[0].source_truncated);
        assert!(matches!(&prepared.receipt.streams[1].content,
            super::super::batch::DeliveredContent::Inline { text, .. } if text.is_empty()));
    }
}
