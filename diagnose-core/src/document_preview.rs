//! An account-scoped preview is paged only on its original device.

use desk_agent_protocol::{OperationOutput, ReadContextOutput};

pub fn has_device_receipt(
    messages: &[crate::chat::ChatMessage],
    device_id: &str,
    preview_id: &str,
    page: u32,
    now_unix_ms: u64,
) -> bool {
    messages.iter().any(|message| {
        let result = message.trusted_tool_result();
        let Some((call_id, envelope)) = result
            .tool_call_id
            .as_deref()
            .zip(result.data_envelope.as_ref())
        else {
            return false;
        };
        if message.tool_ok != Some(true)
            || envelope.provenance.source_provider_id
                != crate::ai_assistant::DOCUMENT_PREVIEW_PROVIDER_ID
            || envelope.provenance.source_tool_name != "preview_document"
            || envelope.provenance.source_object_id.as_deref()
                != Some(&format!("{device_id}:{call_id}"))
            || envelope.validate().is_err()
            || crate::model_egress::envelope_expires_by(envelope, now_unix_ms)
            || envelope.content.size_bytes() != result.text.len() as u64
        {
            return false;
        }
        matches!(serde_json::from_str(&result.text),
            Ok(OperationOutput::ReadContext(ReadContextOutput::DocumentPreview(preview)))
                if preview.preview_id == preview_id && page > 0 && page <= preview.page_count
                    && preview.first_page.is_none())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        chat::ChatMessage,
        model_message_labels::{ReadResultLabel, read_result_envelope},
        seam::{ToolOutputFormat, ToolRunOutput},
    };
    use desk_agent_protocol::document_conversion::DocumentPreviewOutput;

    #[test]
    fn preview_pages_require_an_intact_receipt_from_the_same_device() {
        let call = crate::chat::ToolCall {
            id: "preview-call".into(),
            name: "preview_document".into(),
            arguments_json: "{}".into(),
        };
        let result = OperationOutput::ReadContext(ReadContextOutput::DocumentPreview(
            DocumentPreviewOutput {
                preview_id: "preview".into(),
                source_digest_sha256: "a".repeat(64),
                page_count: 3,
                template_version: "template".into(),
                font_set_sha256: "b".repeat(64),
                engine: "typst".into(),
                warnings: vec![],
                first_page: None,
            },
        ));
        let output = ToolRunOutput {
            format: ToolOutputFormat::Text,
            content: serde_json::to_string(&result).unwrap(),
            image_data_url: None,
            document_preview: None,
        };
        let label = read_result_envelope(
            crate::ai_assistant::ai_assistant_provider_registry_ref(),
            &call,
            &output,
            ReadResultLabel {
                envelope_id: "envelope".into(),
                observation_id: "observation".into(),
                source_object_id: Some("device-a:preview-call".into()),
                observed_at_unix_ms: 1000,
            },
        )
        .unwrap();
        let mut message = ChatMessage::tool_result("message", &call.id, output.content);
        message.tool_ok = Some(true);
        message.data_envelope = Some(label);
        assert!(has_device_receipt(
            std::slice::from_ref(&message),
            "device-a",
            "preview",
            2,
            1001
        ));
        assert!(!has_device_receipt(
            std::slice::from_ref(&message),
            "device-b",
            "preview",
            2,
            1001
        ));
        assert!(!has_device_receipt(
            std::slice::from_ref(&message),
            "device-a",
            "preview",
            4,
            1001
        ));
    }
}
