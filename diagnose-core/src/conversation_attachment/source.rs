//! Producer-side format and size checks, before transport or attachment writes.
use desk_agent_protocol::{AgentError, AgentErrorKind, OperationOutput, ReadContextOutput};

use super::MAX_JSON_BYTES;

/// Exhaustive by design: every new source must explicitly choose its contract.
/// A transport JSON envelope does not make an image or text body a JSON result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    Json,
    FileText,
    TerminalText,
    Image,
    Command,
}

pub fn source_format(output: &OperationOutput) -> SourceFormat {
    match output {
        OperationOutput::Exec(_) => SourceFormat::Command,
        OperationOutput::ReadContext(output) => match output {
            ReadContextOutput::FileContentRead(_) => SourceFormat::FileText,
            ReadContextOutput::TerminalOutputInspect(_) => SourceFormat::TerminalText,
            ReadContextOutput::ScreenCaptureCurrent(_) => SourceFormat::Image,
            ReadContextOutput::SystemInfo(_)
            | ReadContextOutput::ProcessList(_)
            | ReadContextOutput::NetworkPorts(_)
            | ReadContextOutput::ServiceStatus(_)
            | ReadContextOutput::LogRecent(_)
            | ReadContextOutput::ContainerList(_)
            | ReadContextOutput::ContainerInspect(_)
            | ReadContextOutput::ContainerLogs(_)
            | ReadContextOutput::DesktopSessionInspect(_)
            | ReadContextOutput::DesktopUiInspect(_)
            | ReadContextOutput::OfficeDocumentInspect(_)
            | ReadContextOutput::SpreadsheetLiveInspect(_)
            | ReadContextOutput::DocumentLiveInspect(_)
            | ReadContextOutput::PresentationLiveInspect(_)
            | ReadContextOutput::FileMetadataInspect(_)
            | ReadContextOutput::FileDirectoryResolve(_)
            | ReadContextOutput::SpreadsheetFileInspect(_)
            | ReadContextOutput::SpreadsheetMergePreview(_)
            | ReadContextOutput::ApplicationList(_)
            | ReadContextOutput::ApplicationLaunchResolve(_) => SourceFormat::Json,
        },
    }
}

/// Count the final compact encoded result, including escaping and wrapper keys.
/// Source capture and authorization ceilings remain independently enforced.
pub fn validate_output(output: &OperationOutput) -> Result<(), AgentError> {
    match source_format(output) {
        SourceFormat::Json => {
            let bytes = serde_json::to_vec(output).map_err(|_| AgentError {
                kind: AgentErrorKind::Internal,
                message: "Unable to encode the typed tool result".into(),
                retryable: false,
                safe_for_model: false,
                error_code: None,
            })?;
            if bytes.len() > MAX_JSON_BYTES {
                return Err(AgentError {
                    kind: AgentErrorKind::OutputLimitExceeded,
                    message: "JSON result exceeds 32768 UTF-8 bytes. No partial JSON was returned. Narrow the source tool's supported search, root, range or count parameters. Do not repeat a side-effecting action to obtain a smaller result.".into(),
                    retryable: false,
                    safe_for_model: true,
                    error_code: None,
                });
            }
            Ok(())
        }
        SourceFormat::FileText
        | SourceFormat::TerminalText
        | SourceFormat::Image
        | SourceFormat::Command => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{ContainerInspectOutput, ExecOutput, ExecOutputStreams};

    #[test]
    fn final_json_boundary_counts_wrappers_and_escaping() {
        let mut output = OperationOutput::ReadContext(ReadContextOutput::ContainerInspect(
            ContainerInspectOutput {
                container_id: "id".into(),
                details_json: "{}".into(),
                redactions: vec![],
                truncated: false,
            },
        ));
        let overhead = serde_json::to_vec(&output).unwrap().len();
        let OperationOutput::ReadContext(ReadContextOutput::ContainerInspect(ref mut body)) =
            output
        else {
            unreachable!()
        };
        body.details_json = format!("{{{}}}", " ".repeat(MAX_JSON_BYTES - overhead));
        assert_eq!(serde_json::to_vec(&output).unwrap().len(), MAX_JSON_BYTES);
        validate_output(&output).unwrap();
        let OperationOutput::ReadContext(ReadContextOutput::ContainerInspect(ref mut body)) =
            output
        else {
            unreachable!()
        };
        body.details_json.push(' ');
        assert_eq!(
            validate_output(&output).unwrap_err().kind,
            AgentErrorKind::OutputLimitExceeded
        );
    }

    #[test]
    fn command_text_is_not_reclassified_by_json_looking_contents() {
        let output = OperationOutput::Exec(ExecOutput {
            exit_code: 0,
            duration_ms: 1,
            redactions: vec![],
            streams: ExecOutputStreams::Split {
                stdout: format!("{{\"data\":\"{}\"}}", "x".repeat(40000)),
                stderr: String::new(),
                stdout_truncated: false,
                stderr_truncated: false,
            },
        });
        assert_eq!(source_format(&output), SourceFormat::Command);
        validate_output(&output).unwrap();
    }
}
