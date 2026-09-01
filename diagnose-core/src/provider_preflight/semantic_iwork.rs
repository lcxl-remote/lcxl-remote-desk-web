//! Closed iWork mutation input bound to the original owner selection.

use super::*;
use crate::input_read_context::{ReadContextSelection, live_read};
use desk_agent_protocol::computer_use::{
    BatchDocumentOutput, ComputerUseAdapterKind, DocumentLiveBatchPatchAction,
    DocumentLivePatchAction, PresentationLiveBatchPatchAction, PresentationLivePatchAction,
    SpreadsheetLiveBatchPatchAction, SpreadsheetLivePatchAction,
};

fn unavailable() -> AgentError {
    error(
        AgentErrorKind::PermissionDenied,
        "iWork input or original object selection is unavailable",
        false,
        true,
    )
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SpreadsheetActionArgs {
    target: ObjectRef,
    action: SpreadsheetLivePatchAction,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DocumentActionArgs {
    target: ObjectRef,
    text: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PresentationActionArgs {
    target: ObjectRef,
    action: PresentationLivePatchAction,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SpreadsheetBatchActionArgs {
    target: ObjectRef,
    output: BatchDocumentOutput,
    action: SpreadsheetLivePatchAction,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DocumentBatchActionArgs {
    target: ObjectRef,
    output: BatchDocumentOutput,
    text: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PresentationBatchActionArgs {
    target: ObjectRef,
    output: BatchDocumentOutput,
    action: PresentationLivePatchAction,
}

/// Parsing never refreshes a reference. The runtime still rechecks the stored
/// input, current session and connection-fenced readiness before each send.
pub struct IworkCallPreflight {
    target: ObjectRef,
    action: ComputerActionKind,
    adapter_kind: ComputerUseAdapterKind,
    capability: CapabilityDescriptor,
    provider_id: String,
    surface: ProductSurface,
    canonical_input_json: String,
    canonical_input_digest_sha256: String,
    resource_scope: Vec<String>,
    operation_scope: Vec<String>,
    risk_tier: CapabilityRiskTier,
    valid_until_unix_ms: u64,
}

impl IworkCallPreflight {
    pub fn supports(tool_name: &str) -> bool {
        matches!(
            tool_name,
            "patch_live_spreadsheet_cell"
                | "replace_live_document_body"
                | "patch_live_presentation_slide"
                | "patch_selected_numbers_copy"
                | "replace_selected_pages_copy_body"
                | "patch_selected_keynote_copy"
        )
    }

    pub fn build(
        registry: &ProviderRegistry,
        surface: ProductSurface,
        call: &ToolCall,
        original: &ReadContextSelection,
        now_unix_ms: u64,
    ) -> Result<Self, AgentError> {
        original.validate()?;
        let capability = registry
            .capability_for_tool(&call.name)
            .ok_or_else(unavailable)?;
        let provider = registry
            .provider_for_capability(&capability.wire.capability_id)
            .ok_or_else(unavailable)?;
        if !Self::supports(&call.name)
            || !matches!(
                surface,
                ProductSurface::OssPersonalOwner | ProductSurface::ManagerPersonalOwner
            )
            || !capability.wire.surfaces.contains(&surface)
            || capability.wire.authorization_hint.resources
                != [AuthorizationResourceKind::FreshObjectReference]
            || call.arguments_json.len() > capability.wire.limits.max_input_bytes as usize
            || now_unix_ms == 0
        {
            return Err(unavailable());
        }

        let selected_refs = original
            .object_attachments
            .iter()
            .map(|attachment| {
                if !attachment.is_active_at(now_unix_ms) {
                    return Err(unavailable());
                }
                serde_json::from_str::<ObjectRef>(&attachment.object_ref.opaque_token)
                    .map_err(|_| unavailable())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let exact_batch_file = || {
            let files = selected_refs
                .iter()
                .filter(|reference| reference.object_kind == ObjectKind::File)
                .collect::<Vec<_>>();
            match files.as_slice() {
                [file] => Ok((*file).clone()),
                _ => Err(unavailable()),
            }
        };
        let validate_destination = |destination: &ObjectRef| {
            if destination.object_kind != ObjectKind::Directory
                || !selected_refs.contains(destination)
            {
                return Err(unavailable());
            }
            Ok(())
        };

        let (target, authority_refs, action, adapter_kind, mut valid_until_unix_ms) = match call
            .name
            .as_str()
        {
            "patch_live_spreadsheet_cell" => {
                let args: SpreadsheetActionArgs =
                    serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
                let frozen = live_read::target(original, "inspect_live_spreadsheet", now_unix_ms)?;
                if frozen.object_ref != args.target {
                    return Err(unavailable());
                }
                (
                    args.target.clone(),
                    vec![args.target],
                    ComputerActionKind::SpreadsheetLive(args.action),
                    ComputerUseAdapterKind::IworkNumbers,
                    live_read::expiry(original, frozen)?,
                )
            }
            "replace_live_document_body" => {
                let args: DocumentActionArgs =
                    serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
                let frozen = live_read::target(original, "inspect_live_document", now_unix_ms)?;
                if frozen.object_ref != args.target {
                    return Err(unavailable());
                }
                (
                    args.target.clone(),
                    vec![args.target],
                    ComputerActionKind::DocumentLive(DocumentLivePatchAction::ReplaceBodyText {
                        text: args.text,
                    }),
                    ComputerUseAdapterKind::IworkPages,
                    live_read::expiry(original, frozen)?,
                )
            }
            "patch_live_presentation_slide" => {
                let args: PresentationActionArgs =
                    serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
                let frozen = live_read::target(original, "inspect_live_presentation", now_unix_ms)?;
                if frozen.object_ref != args.target {
                    return Err(unavailable());
                }
                (
                    args.target.clone(),
                    vec![args.target],
                    ComputerActionKind::PresentationLive(args.action),
                    ComputerUseAdapterKind::IworkKeynote,
                    live_read::expiry(original, frozen)?,
                )
            }
            "patch_selected_numbers_copy" => {
                let args: SpreadsheetBatchActionArgs =
                    serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
                if exact_batch_file()? != args.target {
                    return Err(unavailable());
                }
                validate_destination(&args.output.destination_parent)?;
                let refs = vec![args.target.clone(), args.output.destination_parent.clone()];
                (
                    args.target,
                    refs,
                    ComputerActionKind::SpreadsheetLiveBatch(SpreadsheetLiveBatchPatchAction {
                        output: args.output,
                        action: args.action,
                    }),
                    ComputerUseAdapterKind::IworkNumbers,
                    u64::MAX,
                )
            }
            "replace_selected_pages_copy_body" => {
                let args: DocumentBatchActionArgs =
                    serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
                if exact_batch_file()? != args.target {
                    return Err(unavailable());
                }
                validate_destination(&args.output.destination_parent)?;
                let refs = vec![args.target.clone(), args.output.destination_parent.clone()];
                (
                    args.target,
                    refs,
                    ComputerActionKind::DocumentLiveBatch(DocumentLiveBatchPatchAction {
                        output: args.output,
                        action: DocumentLivePatchAction::ReplaceBodyText { text: args.text },
                    }),
                    ComputerUseAdapterKind::IworkPages,
                    u64::MAX,
                )
            }
            "patch_selected_keynote_copy" => {
                let args: PresentationBatchActionArgs =
                    serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
                if exact_batch_file()? != args.target {
                    return Err(unavailable());
                }
                validate_destination(&args.output.destination_parent)?;
                let refs = vec![args.target.clone(), args.output.destination_parent.clone()];
                (
                    args.target,
                    refs,
                    ComputerActionKind::PresentationLiveBatch(PresentationLiveBatchPatchAction {
                        output: args.output,
                        action: args.action,
                    }),
                    ComputerUseAdapterKind::IworkKeynote,
                    u64::MAX,
                )
            }
            _ => return Err(unavailable()),
        };
        if action.required_capability() != capability.required_capability {
            return Err(unavailable());
        }
        for reference in &authority_refs {
            let expiry = chrono::DateTime::parse_from_rfc3339(&reference.expires_at)
                .ok()
                .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
                .filter(|expiry| *expiry > now_unix_ms)
                .ok_or_else(unavailable)?;
            valid_until_unix_ms = valid_until_unix_ms.min(expiry);
        }
        if let Some(expiry) = &original.expires_at {
            valid_until_unix_ms = valid_until_unix_ms.min(
                chrono::DateTime::parse_from_rfc3339(expiry)
                    .ok()
                    .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
                    .filter(|expiry| *expiry > now_unix_ms)
                    .ok_or_else(unavailable)?,
            );
        }
        if valid_until_unix_ms <= now_unix_ms {
            return Err(unavailable());
        }
        let canonical_input_json = canonical_tool_permission_input_json(
            &call.name,
            serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?,
        )
        .map_err(|_| unavailable())?;
        let operation_scope = canonical_compiled_scope(
            &capability.wire.authorization_hint.resources,
            capability.wire.effect,
        )
        .ok_or_else(unavailable)?
        .operations;
        Ok(Self {
            target,
            action,
            adapter_kind,
            capability: capability.clone(),
            provider_id: provider.wire.provider_id.clone(),
            surface,
            canonical_input_digest_sha256: format!(
                "{:x}",
                Sha256::digest(canonical_input_json.as_bytes())
            ),
            canonical_input_json,
            resource_scope: fresh_object_resource_scope(&authority_refs),
            operation_scope,
            risk_tier: classify_provider_call(capability, call)?,
            valid_until_unix_ms,
        })
    }

    pub fn target(&self) -> &ObjectRef {
        &self.target
    }

    pub fn action(&self) -> &ComputerActionKind {
        &self.action
    }

    pub fn adapter_kind(&self) -> ComputerUseAdapterKind {
        self.adapter_kind
    }

    pub fn canonical_input_json(&self) -> &str {
        &self.canonical_input_json
    }

    pub fn required_capability(&self) -> Capability {
        self.capability.required_capability
    }

    pub fn valid_until_unix_ms(&self) -> u64 {
        self.valid_until_unix_ms
    }

    pub fn resource_scope(&self) -> &[String] {
        &self.resource_scope
    }

    pub fn grant_call<'a>(
        &'a self,
        subject: &'a ProviderCallSubject<'_>,
    ) -> Result<CapabilityGrantCall<'a>, AgentError> {
        crate::assistant_policy::require_current_policy(subject.policy_revision)?;
        if subject.readiness_revision == 0
            || subject.now_unix_ms == 0
            || subject.now_unix_ms >= self.valid_until_unix_ms
            || [subject.actor_id, subject.run_id, subject.target_device_id]
                .iter()
                .any(|id| id.trim().is_empty())
        {
            return Err(unavailable());
        }
        Ok(CapabilityGrantCall {
            actor_id: subject.actor_id,
            run_id: subject.run_id,
            surface: self.surface,
            target_device_id: subject.target_device_id,
            target_session_id: None,
            provider_id: &self.provider_id,
            capability_id: &self.capability.wire.capability_id,
            tool_name: &self.capability.wire.tool_name,
            tool_schema_version: self.capability.wire.input_schema_version,
            effect: self.capability.wire.effect,
            risk_tier: self.risk_tier,
            resource_scope: &self.resource_scope,
            operation_scope: &self.operation_scope,
            export_destinations: &[],
            envelope_ids: &[],
            content_digests_sha256: &[],
            canonical_input_digest_sha256: &self.canonical_input_digest_sha256,
            byte_count: self.canonical_input_json.len() as u64,
            item_count: 1,
            policy_revision: subject.policy_revision,
            readiness_revision: subject.readiness_revision,
            now_unix_ms: subject.now_unix_ms,
        })
    }
}

#[cfg(test)]
mod tests;
