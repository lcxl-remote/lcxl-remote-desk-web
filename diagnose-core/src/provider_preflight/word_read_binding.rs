//! Bind authenticated DOCX observations to the selected file and current worker.
use desk_agent_protocol::{
    AgentError, AgentErrorKind,
    computer_use::{
        BatchDocumentSourceProjection, ComputerUseAdapterRef, LiveDocumentInspectOutput,
        LiveDocumentProjection, ObjectKind, ObjectRef, office_batch,
    },
};
use sha2::{Digest, Sha256};

pub struct WordReadBinding {
    source: BatchDocumentSourceProjection,
    target: ObjectRef,
    adapter: ComputerUseAdapterRef,
    valid_until: u64,
}

impl WordReadBinding {
    /// Callers supply authenticated device output and authoritative selection.
    /// This proof does not grant access or authorize publication.
    pub fn from_authenticated_read(
        selected_file: &ObjectRef,
        worker_incarnation: &str,
        output: &LiveDocumentInspectOutput,
        now: u64,
    ) -> Result<Self, AgentError> {
        let source = output.batch_source.as_ref().ok_or_else(denied)?;
        let LiveDocumentProjection::Document {
            document,
            body_text,
            body_sha256,
        } = &output.projection
        else {
            return Err(denied());
        };
        if !office_batch::is_docx(&output.adapter)
            || selected_file.object_kind != ObjectKind::File
            || &source.file != selected_file
            || source.byte_len == 0
            || source.byte_len > 16 * 1024 * 1024
            || source.sha256.len() != 64
            || !source.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || body_text.len() > 64 * 1024
            || *body_sha256 != format!("{:x}", Sha256::digest(body_text.as_bytes()))
            || document.object_kind != ObjectKind::Document
            || document.token == selected_file.token
            || document.snapshot_id != output.snapshot_id
            || worker_incarnation.is_empty()
            || worker_incarnation.len() > 4096
            || !output
                .snapshot_id
                .strip_prefix(worker_incarnation)
                .and_then(|suffix| suffix.strip_prefix(':'))
                .and_then(|sequence| sequence.parse::<u64>().ok())
                .is_some_and(|sequence| sequence > 0)
        {
            return Err(denied());
        }
        let valid_until = expiry(selected_file, now)?.min(expiry(document, now)?);
        Ok(Self {
            source: source.clone(),
            target: document.clone(),
            adapter: output.adapter.clone(),
            valid_until,
        })
    }

    pub fn source(&self) -> &BatchDocumentSourceProjection {
        &self.source
    }
    pub fn adapter(&self) -> &ComputerUseAdapterRef {
        &self.adapter
    }
    pub fn valid_until_unix_ms(&self) -> u64 {
        self.valid_until
    }
    pub fn validate_target(&self, target: &ObjectRef, now: u64) -> Result<(), AgentError> {
        if target != &self.target || now == 0 || now >= self.valid_until {
            return Err(denied());
        }
        Ok(())
    }
}

/// Resolve only authenticated tool results already held by the owning session.
pub fn resolve_word_read(
    session: &crate::session::PersistedAgentSession,
    selected_file: &ObjectRef,
    worker_incarnation: &str,
    target: &ObjectRef,
    now: u64,
) -> Result<WordReadBinding, AgentError> {
    use crate::{ai_assistant::windows_word, chat::ChatRole};
    let mut selected = None;
    for message in &session.conversation {
        if !matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput) {
            continue;
        }
        let Some(call_id) = message.tool_call_id.as_deref() else {
            continue;
        };
        let mut calls = session
            .conversation
            .iter()
            .filter(|entry| entry.role == ChatRole::Assistant)
            .flat_map(|entry| &entry.tool_calls)
            .filter(|call| call.id == call_id);
        let Some(call) = calls.next() else {
            continue;
        };
        if calls.next().is_some() || call.name != windows_word::INSPECT_TOOL {
            continue;
        }
        let Some(envelope) = &message.trusted_tool_result().data_envelope else {
            continue;
        };
        if envelope.validate().is_err()
            || crate::model_egress::envelope_expires_by(envelope, now)
            || envelope.provenance.source_tool_name != call.name
            || envelope.provenance.source_provider_id != windows_word::PROVIDER_ID
            || envelope.digest_sha256
                != format!(
                    "{:x}",
                    Sha256::digest(message.trusted_tool_result().text.as_bytes())
                )
        {
            continue;
        }
        let Ok(desk_agent_protocol::OperationOutput::ReadContext(
            desk_agent_protocol::ReadContextOutput::DocumentLiveInspect(output),
        )) = serde_json::from_str(&message.trusted_tool_result().text)
        else {
            continue;
        };
        let Ok(binding) = WordReadBinding::from_authenticated_read(
            selected_file,
            worker_incarnation,
            &output,
            now,
        ) else {
            continue;
        };
        if binding.validate_target(target, now).is_err() {
            continue;
        }
        if selected.is_some() {
            return Err(denied());
        }
        selected = Some(binding);
    }
    selected.ok_or_else(denied)
}

pub(super) fn expiry(reference: &ObjectRef, now: u64) -> Result<u64, AgentError> {
    if now == 0
        || reference.token.is_empty()
        || reference.token.len() > 4096
        || reference.snapshot_id.is_empty()
        || reference.snapshot_id.len() > 4096
    {
        return Err(denied());
    }
    chrono::DateTime::parse_from_rfc3339(&reference.expires_at)
        .ok()
        .and_then(|date| u64::try_from(date.timestamp_millis()).ok())
        .filter(|expiry| *expiry > now)
        .ok_or_else(denied)
}

fn denied() -> AgentError {
    AgentError {
        kind: AgentErrorKind::PermissionDenied,
        message: "Word batch read is not bound to the selected file and current worker".into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::computer_use::ComputerUseAdapterKind;

    #[test]
    fn selected_docx_identity_body_and_incarnation_are_bound_until_first_expiry() {
        let object = |kind, token: &str, expires: &str| ObjectRef {
            object_kind: kind,
            token: token.into(),
            snapshot_id: "worker:1".into(),
            expires_at: expires.into(),
        };
        let file = object(ObjectKind::File, "file", "2030-01-01T00:00:00Z");
        let document = object(ObjectKind::Document, "document", "2029-01-01T00:00:00Z");
        let output = LiveDocumentInspectOutput {
            snapshot_id: "worker:1".into(),
            adapter: ComputerUseAdapterRef {
                kind: ComputerUseAdapterKind::OfficeWord,
                version: office_batch::DOCX_ADAPTER_VERSION.into(),
            },
            projection: LiveDocumentProjection::Document {
                document: document.clone(),
                body_text: "正文".into(),
                body_sha256: format!("{:x}", Sha256::digest("正文".as_bytes())),
            },
            batch_source: Some(BatchDocumentSourceProjection {
                file: file.clone(),
                display_name: "source.docx".into(),
                byte_len: 100,
                sha256: "a".repeat(64),
            }),
        };
        let binding =
            WordReadBinding::from_authenticated_read(&file, "worker", &output, 1).unwrap();
        binding.validate_target(&document, 1).unwrap();
        assert!(binding.validate_target(&file, 1).is_err());
        assert!(
            binding
                .validate_target(&document, binding.valid_until_unix_ms())
                .is_err()
        );
        assert!(WordReadBinding::from_authenticated_read(&file, "old-worker", &output, 1).is_err());
        for case in 0..10 {
            let mut bad = output.clone();
            match case {
                0 => bad.batch_source = None,
                1 => bad.batch_source.as_mut().unwrap().file.token = "other".into(),
                2 => bad.batch_source.as_mut().unwrap().byte_len = 16 * 1024 * 1024 + 1,
                3 => bad.batch_source.as_mut().unwrap().sha256 = "invalid".into(),
                4 => bad.adapter.version = "office-docx-batch/v2".into(),
                5 => bad.snapshot_id = "worker:2".into(),
                _ => {
                    let LiveDocumentProjection::Document {
                        document,
                        body_sha256,
                        body_text,
                    } = &mut bad.projection
                    else {
                        unreachable!()
                    };
                    match case {
                        6 => document.token = file.token.clone(),
                        7 => document.expires_at = "1970-01-01T00:00:00Z".into(),
                        8 => *body_sha256 = "b".repeat(64),
                        _ => *body_text = "changed".into(),
                    }
                }
            }
            assert!(
                WordReadBinding::from_authenticated_read(&file, "worker", &bad, 1).is_err(),
                "case {case}"
            );
        }
    }
}
