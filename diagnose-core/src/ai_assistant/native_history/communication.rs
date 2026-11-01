//! Bounded field projection of an original sealed Gmail/Slack send result.
//! Native dispatch/current source/retention authority remains with the host.

use super::*;
use crate::{
    action_result::{ActionResultOrigin, ActionResultReceipt},
    seam::ToolRunOutput,
};
use desk_agent_protocol::{
    communication::{
        CommunicationChannel, CommunicationSurfaceKind, RecipientKind, RecipientRole, SendOutcome,
        SendReceiptEvidence,
    },
    computer_use::ComputerActionCompleted,
    data_lineage::DataEnvelope,
};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeSendHistoryError {
    InvalidOriginal,
    Capacity,
}

pub struct NativeSendHistoryContext<'a> {
    pub origin: &'a ActionResultOrigin,
    pub result: &'a ActionResultReceipt,
    pub output: &'a ToolRunOutput,
    pub provider_device_id: &'a str,
    pub dispatched_at_unix_ms: u64,
    pub original_source_contract_revision: u64,
    pub maximum_bytes: usize,
}

#[derive(Serialize)]
struct Recipient {
    role: RecipientRole,
    kind: RecipientKind,
    stable_id: String,
    canonical_address: String,
    display_name: Option<String>,
    group_member_count: usize,
    member_snapshot_sha256: Option<String>,
}

#[derive(Serialize)]
struct Attachment {
    file_name: String,
    media_type: String,
    size_bytes: u64,
    digest_sha256: String,
    content_ref_sha256: String,
}

#[derive(Serialize)]
struct Fields {
    format_version: u16,
    projection_contract_revision: u64,
    work_id: i64,
    action_request_id: String,
    execution_id: String,
    attempt: u32,
    snapshot_id: String,
    snapshot_sha256: String,
    channel: CommunicationChannel,
    surface_kind: CommunicationSurfaceKind,
    account_id: String,
    subject: String,
    outcome: SendOutcome,
    evidence: SendReceiptEvidence,
    provider_receipt_id: Option<String>,
    provider_observed_at_unix_ms: u64,
    received_at_unix_ms: u64,
    timing_notice: Option<&'static str>,
    recipients: Vec<Recipient>,
    attachments: Vec<Attachment>,
    omitted_recipients: usize,
    omitted_attachments: usize,
    /// Expanded membership is not copied from the sealed payload. The group
    /// identity and original membership digest remain explicit in each entry.
    omitted_group_members: usize,
}

pub struct NativeSendHistoryProjection {
    content: String,
    metadata: NativeHistoryMetadata,
    original_envelope: DataEnvelope,
    recorded_at_unix_ms: u64,
}

impl NativeSendHistoryProjection {
    /// All outcomes remain exact native facts. A draft, arbitrary JSON receipt,
    /// different account/snapshot or replayed clock cannot create this view.
    pub fn from_web_action(
        context: NativeSendHistoryContext<'_>,
        canonical_input: &str,
        completion: &ComputerActionCompleted,
    ) -> Result<Option<Self>, NativeSendHistoryError> {
        use NativeSendHistoryError::{Capacity, InvalidOriginal};
        let NativeSendHistoryContext {
            origin,
            result,
            output,
            provider_device_id,
            dispatched_at_unix_ms,
            original_source_contract_revision,
            maximum_bytes,
        } = context;
        let expected_provider = match origin.tool_name.as_str() {
            "send_gmail_message" => super::super::GMAIL_WEB_SEND_PROVIDER_ID,
            "send_slack_message" => super::super::SLACK_WEB_SEND_PROVIDER_ID,
            _ => return Ok(None),
        };
        if original_source_contract_revision
            != super::super::source_registry::SOURCE_CONTRACT_REVISION
            || origin.provider_id != expected_provider
            || origin.command_completion.is_some()
            || origin.tool_call_id.is_empty()
            || dispatched_at_unix_ms == 0
            || dispatched_at_unix_ms > i64::MAX as u64
            || maximum_bytes == 0
            || !matches!(
                result.action.kind,
                crate::session::WorkKind::CapabilityProvider
                    | crate::session::WorkKind::ComputerAction
            )
            || result.action.action_request_id != completion.action_request_id
            || result.action.work_id.to_string() != completion.work_id
            || result.action.execution_id != completion.execution_generation
            || !matches!(&output.format, crate::seam::ToolOutputFormat::Json)
            || output.image_data_url.is_some()
            || output.document_preview.is_some()
        {
            return Err(InvalidOriginal);
        }
        result
            .validate_for(origin, result.action.clone(), result.attempt, output)
            .map_err(|_| InvalidOriginal)?;
        let receipt = crate::communication_handoff::project_web_send_receipt(
            &origin.tool_name,
            canonical_input,
            completion,
        )
        .map_err(|_| InvalidOriginal)?
        .ok_or(InvalidOriginal)?;
        if crate::communication_handoff::parse_send_receipt_output(&output.content)
            .map_err(|_| InvalidOriginal)?
            .as_ref()
            != Some(&receipt)
        {
            return Err(InvalidOriginal);
        }
        let snapshot = crate::communication_handoff::frozen_web_send_snapshot(
            &origin.tool_name,
            canonical_input,
        )
        .map_err(|_| InvalidOriginal)?
        .ok_or(InvalidOriginal)?;
        if snapshot.run_id != origin.turn_fence.conversation_id
            || snapshot.payload.surface.device_id != provider_device_id
        {
            return Err(InvalidOriginal);
        }
        let received = result.received_at_unix_ms;
        let time = if dispatched_at_unix_ms <= received {
            Some(NativeHistoryTime::Interval {
                from_unix_ms: dispatched_at_unix_ms,
                until_unix_ms: received.checked_add(1).ok_or(InvalidOriginal)?,
                basis: NativeTimeBasis::ProviderAckObserved,
            })
        } else {
            None
        };
        let mut fields = Fields {
            format_version: 1,
            projection_contract_revision: 1,
            work_id: result.action.work_id,
            action_request_id: result.action.action_request_id.clone(),
            execution_id: result.action.execution_id.clone(),
            attempt: result.attempt,
            snapshot_id: snapshot.snapshot_id.clone(),
            snapshot_sha256: snapshot.canonical_payload_sha256.clone(),
            channel: snapshot.payload.surface.channel,
            surface_kind: snapshot.payload.surface.kind,
            account_id: snapshot.payload.surface.account_id.clone(),
            subject: snapshot.payload.subject.clone(),
            outcome: receipt.outcome,
            evidence: receipt.evidence,
            provider_receipt_id: receipt.provider_receipt_id.clone(),
            provider_observed_at_unix_ms: receipt.observed_at_unix_ms,
            received_at_unix_ms: received,
            timing_notice: time.is_none().then_some("native_clock_order_unavailable"),
            recipients: Vec::new(),
            attachments: Vec::new(),
            omitted_recipients: snapshot.payload.recipients.len(),
            omitted_attachments: snapshot.payload.attachments.len(),
            omitted_group_members: snapshot
                .payload
                .recipients
                .iter()
                .map(|value| value.resolved_members.len())
                .sum(),
        };
        if encode(&fields)?.len() > maximum_bytes {
            return Err(Capacity);
        }
        for recipient in &snapshot.payload.recipients {
            fields.recipients.push(Recipient {
                role: recipient.role,
                kind: recipient.kind,
                stable_id: recipient.stable_id.clone(),
                canonical_address: recipient.canonical_address.clone(),
                display_name: recipient.display_name.clone(),
                group_member_count: recipient.resolved_members.len(),
                member_snapshot_sha256: recipient.member_snapshot_sha256.clone(),
            });
            fields.omitted_recipients -= 1;
            if encode(&fields)?.len() > maximum_bytes {
                fields.recipients.pop();
                fields.omitted_recipients += 1;
                break;
            }
        }
        for attachment in &snapshot.payload.attachments {
            fields.attachments.push(Attachment {
                file_name: attachment.file_name.clone(),
                media_type: attachment.media_type.clone(),
                size_bytes: attachment.size_bytes,
                digest_sha256: attachment.digest_sha256.clone(),
                content_ref_sha256: crate::model_egress::sink_metadata_sha256(&attachment.content)
                    .map_err(|_| InvalidOriginal)?,
            });
            fields.omitted_attachments -= 1;
            if encode(&fields)?.len() > maximum_bytes {
                fields.attachments.pop();
                fields.omitted_attachments += 1;
                break;
            }
        }
        let content = encode(&fields)?;
        if content.len() > maximum_bytes {
            return Err(Capacity);
        }
        let receipt_sha256 = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(result).map_err(|_| InvalidOriginal)?)
        );
        let native_proof_sha256 = crate::model_egress::sink_metadata_sha256(&(
            "native-send-history-fields-v1",
            original_source_contract_revision,
            &result.origin_digest_sha256,
            &receipt_sha256,
            &snapshot.canonical_payload_sha256,
            provider_device_id,
            dispatched_at_unix_ms,
        ))
        .map_err(|_| InvalidOriginal)?;
        let metadata = NativeHistoryMetadata {
            format_version: 1,
            native_proof_sha256,
            projected_content_sha256: format!("{:x}", Sha256::digest(content.as_bytes())),
            time,
            status_snapshot: None,
            projection_partial: fields.omitted_recipients > 0
                || fields.omitted_attachments > 0
                || fields.omitted_group_members > 0,
        };
        metadata
            .validate(
                AssistantSourceKind::Receipt,
                &metadata.projected_content_sha256,
                received,
            )
            .map_err(|_| InvalidOriginal)?;
        Ok(Some(Self {
            content,
            metadata,
            original_envelope: result.envelope.clone(),
            recorded_at_unix_ms: received,
        }))
    }

    pub fn content(&self) -> &str {
        &self.content
    }
    pub fn metadata(&self) -> &NativeHistoryMetadata {
        &self.metadata
    }
    pub fn original_envelope(&self) -> &DataEnvelope {
        &self.original_envelope
    }
    pub fn recorded_at_unix_ms(&self) -> u64 {
        self.recorded_at_unix_ms
    }
}

fn encode(fields: &Fields) -> Result<String, NativeSendHistoryError> {
    serde_json::to_string(fields).map_err(|_| NativeSendHistoryError::InvalidOriginal)
}

#[cfg(test)]
mod tests;
