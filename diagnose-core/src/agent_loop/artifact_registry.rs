//! Metadata checks for reusing verified artifacts; this does not grant access.
use desk_agent_protocol::computer_use::{BatchDocumentArtifact, ObjectKind, ObjectRef};

pub(super) fn live_reference(file: &ObjectRef, now: u64) -> bool {
    file.object_kind == ObjectKind::File
        && !file.token.is_empty()
        && !file.snapshot_id.is_empty()
        && chrono::DateTime::parse_from_rfc3339(&file.expires_at)
            .ok()
            .and_then(|expiry| u64::try_from(expiry.timestamp_millis()).ok())
            .is_some_and(|expiry| {
                expiry > now.saturating_add(crate::model_egress::MODEL_CALL_RETENTION_HEADROOM_MS)
            })
}

pub(super) fn valid_batch_artifact(artifact: &BatchDocumentArtifact, now: u64) -> bool {
    let name = &artifact.file_name;
    let valid_name = [".pages", ".numbers", ".key"]
        .iter()
        .any(|extension| name.ends_with(extension) && name.len() > extension.len());
    let sha = |value: &str| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    live_reference(&artifact.file, now)
        && valid_name
        && name.len() <= 255
        && !name
            .chars()
            .any(|character| character.is_control() || "/\\".contains(character))
        && artifact.byte_len > 0
        && artifact.validation_byte_len > 0
        && sha(&artifact.sha256)
        && sha(&artifact.validation_sha256)
}
