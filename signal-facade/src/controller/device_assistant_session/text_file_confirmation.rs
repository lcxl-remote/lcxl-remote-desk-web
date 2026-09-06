//! Owner-only review of persisted exact file inputs. No references or grants
//! are minted here; dispatch remains responsible for current authorization.
use desk_agent_protocol::computer_use::{
    ComputerActionCompleted, ComputerActionOutput, ComputerActionResultClass, TextFileChange,
    TextFileMutationOperation,
};
use desk_diagnose_core::chat::{ChatMessage, ChatRole};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TextFileConfirmationDto {
    pub file_name: String,
    pub file_result_call_id: String,
    pub expected_sha256: String,
    pub operation: TextFileMutationOperation,
    pub change: Option<TextFileChange>,
    pub one_shot: bool,
    pub recoverable: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    file_result_call_id: String,
    expected_sha256: String,
    #[serde(default)]
    directory_request_id: Option<String>,
    #[serde(default)]
    change: Option<TextFileChange>,
}

pub(super) fn project(
    tool: &str,
    canonical: Option<&str>,
    messages: &[ChatMessage],
) -> Option<TextFileConfirmationDto> {
    let operation = match tool {
        "update_text_file" => TextFileMutationOperation::Update,
        "delete_text_file" => TextFileMutationOperation::Delete,
        _ => return None,
    };
    let canonical = canonical.filter(|text| {
        text.len() <= desk_diagnose_core::dynamic_run::MAX_PERMISSION_EXACT_INPUT_BYTES
    })?;
    let input: Input = serde_json::from_str(canonical).ok()?;
    if input.file_result_call_id.is_empty()
        || input.file_result_call_id.len() > 256
        || input.expected_sha256.len() != 64
        || !input
            .expected_sha256
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        || input
            .directory_request_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 256)
    {
        return None;
    }
    let valid_text = |text: &str| text.len() <= 65_536 && !text.contains('\0');
    match (&operation, &input.change) {
        (TextFileMutationOperation::Delete, None) => {}
        (TextFileMutationOperation::Update, Some(TextFileChange::ReplaceAll { content_utf8 }))
            if valid_text(content_utf8) => {}
        (
            TextFileMutationOperation::Update,
            Some(TextFileChange::ReplaceOnce { before, after }),
        ) if !before.is_empty() && valid_text(before) && valid_text(after) => {}
        _ => return None,
    }
    let mut sources = messages.iter().filter(|message| {
        matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput)
            && message.tool_call_id.as_deref() == Some(input.file_result_call_id.as_str())
    });
    let source = sources.next()?;
    if sources.next().is_some() {
        return None;
    }
    let envelope = source.data_envelope.as_ref()?;
    envelope.validate().ok()?;
    if envelope.digest_sha256 != format!("{:x}", Sha256::digest(source.text.as_bytes())) {
        return None;
    }
    let (file_name, digest) = if envelope.provenance.source_tool_name == "read_selected_text_file"
        && envelope.provenance.source_provider_id == "file.content"
    {
        let desk_agent_protocol::OperationOutput::ReadContext(
            desk_agent_protocol::ReadContextOutput::FileContentRead(read),
        ) = serde_json::from_str(&source.text).ok()?
        else {
            return None;
        };
        if read.byte_len != read.content_utf8.len() as u64
            || read.sha256 != format!("{:x}", Sha256::digest(read.content_utf8.as_bytes()))
        {
            return None;
        }
        (read.display_name, read.sha256)
    } else {
        let completed: ComputerActionCompleted = serde_json::from_str(&source.text).ok()?;
        if completed.result != ComputerActionResultClass::Verified {
            return None;
        }
        let artifact = match completed.output? {
            ComputerActionOutput::FileArtifact(artifact)
                if (envelope.provenance.source_provider_id == "file.artifact"
                    && envelope.provenance.source_tool_name
                        == "create_text_artifact_in_selected_directory")
                    || (envelope.provenance.source_provider_id == "communication.local_draft"
                        && envelope.provenance.source_tool_name
                            == "create_local_communication_draft") =>
            {
                artifact
            }
            ComputerActionOutput::TextFileMutation(result)
                if envelope.provenance.source_provider_id == "file.text"
                    && envelope.provenance.source_tool_name == "update_text_file"
                    && result.verified =>
            {
                result.updated_file?
            }
            _ => return None,
        };
        artifact.validate().ok()?;
        (artifact.file_name, artifact.digest_sha256)
    };
    if file_name.is_empty()
        || file_name.len() > 512
        || file_name.chars().any(char::is_control)
        || digest != input.expected_sha256
    {
        return None;
    }
    Some(TextFileConfirmationDto {
        file_name,
        file_result_call_id: input.file_result_call_id,
        expected_sha256: input.expected_sha256,
        operation,
        change: input.change,
        one_shot: true,
        recoverable: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Vec<ChatMessage>, serde_json::Value) {
        let digest = format!("{:x}", Sha256::digest("原文".as_bytes()));
        let text = serde_json::json!({"ReadContext":{"FileContentRead":{
            "file":{"token":"file","snapshot_id":"snapshot","object_kind":"file","expires_at":"2099-01-01T00:00:00Z"},
            "display_name":"notes.txt","content_utf8":"原文","byte_len":6,"sha256":digest
        }}}).to_string();
        let owner = desk_diagnose_core::model_message_labels::model_bound_user_message(
            "owner".into(),
            "Update the selected file".into(),
            desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: "model".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
        )
        .unwrap();
        let mut receipt = ChatMessage::tool_result("receipt", "read-call", text.clone());
        receipt.data_envelope =
            desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
                owner.data_envelope.as_ref(),
                "read-call",
                &text,
                "read_selected_text_file",
            )
            .unwrap();
        receipt
            .data_envelope
            .as_mut()
            .unwrap()
            .provenance
            .source_provider_id = "file.content".into();
        (
            vec![receipt],
            serde_json::json!({"file_result_call_id":"read-call","expected_sha256":digest,
            "change":{"kind":"replace_once","before":"原文","after":"新文"}}),
        )
    }

    #[test]
    fn exact_file_review_preserves_change_and_uses_verified_source_name() {
        let (messages, mut input) = fixture();
        let review = project("update_text_file", Some(&input.to_string()), &messages).unwrap();
        assert_eq!(review.file_name, "notes.txt");
        assert!(review.one_shot && review.recoverable);
        assert_eq!(
            review.change,
            Some(TextFileChange::ReplaceOnce {
                before: "原文".into(),
                after: "新文".into()
            })
        );
        input["change"] = serde_json::json!({"kind":"replace_all","content_utf8":""});
        assert!(project("update_text_file", Some(&input.to_string()), &messages).is_some());
        input.as_object_mut().unwrap().remove("change");
        let deleted = project("delete_text_file", Some(&input.to_string()), &messages).unwrap();
        assert_eq!(deleted.operation, TextFileMutationOperation::Delete);
        assert!(deleted.change.is_none());
    }

    #[test]
    fn missing_or_forged_file_review_is_not_approvable() {
        let (mut messages, input) = fixture();
        assert!(project("update_text_file", None, &messages).is_none());
        assert!(project("delete_text_file", Some(&input.to_string()), &messages).is_none());
        assert!(project("update_text_file", Some(&input.to_string()), &[]).is_none());
        messages[0].role = ChatRole::User;
        assert!(project("update_text_file", Some(&input.to_string()), &messages).is_none());
        messages[0].role = ChatRole::Tool;
        messages[0].text.push(' ');
        assert!(project("update_text_file", Some(&input.to_string()), &messages).is_none());
    }
}
