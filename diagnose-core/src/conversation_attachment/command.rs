//! Command streams remain distinct from the bounded execution receipt.
use super::batch::{
    DeliveredPart, DeliveryIdentity, OutputPart, PartContent, PreparedAttachment, prepare_delivery,
};
use desk_agent_protocol::{AgentError, ExecOutput, ExecOutputStreams};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandReceipt {
    pub exit_code: i32,
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
            exit_code: output.exit_code,
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
    fn typed_pty_result_does_not_invent_split_streams() {
        let prepared = prepare_command(
            &identity(),
            ExecOutput {
                exit_code: 17,
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
        assert_eq!(prepared.receipt.exit_code, 17);
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
                exit_code: 0,
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
