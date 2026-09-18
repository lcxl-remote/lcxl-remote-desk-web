//! Closed document mutation input bound to the original owner selection.

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
        "document input or original object selection is unavailable",
        false,
        true,
    )
}

const MAX_OBJECT_REF_COMPONENT_BYTES: usize = 4096;
const MAX_DERIVED_OBJECT_REF_TTL_MS: u64 = 300_000;

fn object_ref_expiry(reference: &ObjectRef, now_unix_ms: u64) -> Result<u64, AgentError> {
    chrono::DateTime::parse_from_rfc3339(&reference.expires_at)
        .ok()
        .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
        .filter(|expiry| {
            *expiry > now_unix_ms
                && *expiry <= now_unix_ms.saturating_add(MAX_DERIVED_OBJECT_REF_TTL_MS)
        })
        .ok_or_else(unavailable)
}

/// A live mutation may use either the frozen input reference or the fresh
/// reference returned by its matching semantic read. The latter is accepted
/// only when its snapshot is from the same worker incarnation; the edge still
/// has to resolve the opaque token immediately before applying the action.
fn live_mutation_expiry(
    selection: &ReadContextSelection,
    frozen: &crate::input_read_context::live_read::LiveReadTarget,
    target: &ObjectRef,
    expected_kind: ObjectKind,
    now_unix_ms: u64,
) -> Result<u64, AgentError> {
    if target == &frozen.object_ref {
        return live_read::expiry(selection, frozen);
    }
    if target.object_kind != expected_kind
        || [target.token.as_str(), target.snapshot_id.as_str()]
            .iter()
            .any(|value| value.trim().is_empty() || value.len() > MAX_OBJECT_REF_COMPONENT_BYTES)
    {
        return Err(unavailable());
    }
    let (_, worker_incarnation) = frozen
        .interactive_session_incarnation
        .split_once(':')
        .ok_or_else(unavailable)?;
    target
        .snapshot_id
        .strip_prefix(worker_incarnation)
        .and_then(|suffix| suffix.strip_prefix(':'))
        .filter(|suffix| !suffix.is_empty() && !suffix.contains(':'))
        .and_then(|suffix| suffix.parse::<u64>().ok())
        .filter(|sequence| *sequence > 0)
        .ok_or_else(unavailable)?;
    object_ref_expiry(target, now_unix_ms)
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
                | "patch_numbers_copy"
                | "replace_pages_copy_body"
                | "patch_keynote_copy"
                | "patch_powerpoint_copy"
                | "replace_word_copy_body"
                | "patch_excel_copy"
        )
    }

    pub fn build(
        registry: &ProviderRegistry,
        surface: ProductSurface,
        call: &ToolCall,
        original: &ReadContextSelection,
        approved_directories: &[ObjectRef],
        now_unix_ms: u64,
    ) -> Result<Self, AgentError> {
        Self::build_with_presentation_binding(
            registry,
            surface,
            call,
            original,
            approved_directories,
            now_unix_ms,
            None,
        )
    }

    /// Check frozen-plan consistency only. This never grants authority; send
    /// admission must separately call `from_session` with current device state.
    pub fn frozen_presentation_resources(
        call: &ToolCall,
        target: &ObjectRef,
        action: &ComputerActionKind,
    ) -> Result<Vec<String>, AgentError> {
        if call.name == crate::device_assistant::windows_excel::PATCH_TOOL {
            let args: SpreadsheetBatchActionArgs =
                serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
            let expected =
                ComputerActionKind::SpreadsheetLiveBatch(SpreadsheetLiveBatchPatchAction {
                    output: args.output.clone(),
                    action: args.action,
                });
            if &args.target != target
                || target.object_kind != ObjectKind::Range
                || args.output.destination_parent.object_kind != ObjectKind::Directory
                || &expected != action
            {
                return Err(unavailable());
            }
            return Ok(fresh_object_resource_scope(&[
                args.target,
                args.output.destination_parent,
            ]));
        }
        if call.name == crate::device_assistant::windows_word::PATCH_TOOL {
            let args: DocumentBatchActionArgs =
                serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
            let expected = ComputerActionKind::DocumentLiveBatch(DocumentLiveBatchPatchAction {
                output: args.output.clone(),
                action: DocumentLivePatchAction::ReplaceBodyText { text: args.text },
            });
            if &args.target != target
                || target.object_kind != ObjectKind::Document
                || args.output.destination_parent.object_kind != ObjectKind::Directory
                || &expected != action
            {
                return Err(unavailable());
            }
            return Ok(fresh_object_resource_scope(&[
                args.target,
                args.output.destination_parent,
            ]));
        }
        if !matches!(
            call.name.as_str(),
            "patch_keynote_copy" | "patch_powerpoint_copy"
        ) {
            return Err(unavailable());
        }
        let args: PresentationBatchActionArgs =
            serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
        let expected =
            ComputerActionKind::PresentationLiveBatch(PresentationLiveBatchPatchAction {
                output: args.output.clone(),
                action: args.action,
            });
        if &args.target != target
            || target.object_kind != ObjectKind::Slide
            || args.output.destination_parent.object_kind != ObjectKind::Directory
            || &expected != action
        {
            return Err(unavailable());
        }
        Ok(fresh_object_resource_scope(&[
            args.target,
            args.output.destination_parent,
        ]))
    }

    /// Runtime entry point: directory consent and read evidence come from the
    /// same authoritative session, with the worker supplied by fresh readiness.
    pub fn from_session(
        registry: &ProviderRegistry,
        surface: ProductSurface,
        call: &ToolCall,
        original: &ReadContextSelection,
        session: &crate::session::PersistedAgentSession,
        interactive_session_incarnation: &str,
        now_unix_ms: u64,
    ) -> Result<Self, AgentError> {
        let binding = if matches!(
            call.name.as_str(),
            "patch_keynote_copy" | "patch_powerpoint_copy"
        ) {
            original.validate()?;
            let args: PresentationBatchActionArgs =
                serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
            let (_, worker) = interactive_session_incarnation
                .split_once(':')
                .ok_or_else(unavailable)?;
            let files = super::text_file::batch_source_files(
                session,
                &["inspect_keynote_file", "inspect_powerpoint_file"],
                now_unix_ms,
            );
            let mut bindings = files.iter().filter_map(|file| {
                super::batch_document::resolve_presentation_read(
                    session,
                    &file,
                    worker,
                    &args.target,
                    now_unix_ms,
                )
                .ok()
            });
            let binding = bindings.next().ok_or_else(unavailable)?;
            if bindings.next().is_some() {
                return Err(unavailable());
            }
            Some(binding)
        } else {
            None
        };
        let word = if call.name == crate::device_assistant::windows_word::PATCH_TOOL {
            original.validate()?;
            let args: DocumentBatchActionArgs =
                serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
            let (_, worker) = interactive_session_incarnation
                .split_once(':')
                .ok_or_else(unavailable)?;
            let files = super::text_file::batch_source_files(
                session,
                &[crate::device_assistant::windows_word::INSPECT_TOOL],
                now_unix_ms,
            );
            let mut bindings = files.iter().filter_map(|file| {
                super::word_read_binding::resolve_word_read(
                    session,
                    &file,
                    worker,
                    &args.target,
                    now_unix_ms,
                )
                .ok()
            });
            let binding = bindings.next().ok_or_else(unavailable)?;
            if bindings.next().is_some() {
                return Err(unavailable());
            }
            Some(binding)
        } else {
            None
        };
        let excel = if call.name == crate::device_assistant::windows_excel::PATCH_TOOL {
            original.validate()?;
            let args: SpreadsheetBatchActionArgs =
                serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
            let (_, worker) = interactive_session_incarnation
                .split_once(':')
                .ok_or_else(unavailable)?;
            let files = super::text_file::batch_source_files(
                session,
                &[crate::device_assistant::windows_excel::INSPECT_TOOL],
                now_unix_ms,
            );
            let mut bindings = files.iter().filter_map(|file| {
                super::excel_read_binding::resolve_excel_read(
                    session,
                    &file,
                    worker,
                    &args.target,
                    now_unix_ms,
                )
                .ok()
            });
            let binding = bindings.next().ok_or_else(unavailable)?;
            if bindings.next().is_some() {
                return Err(unavailable());
            }
            Some(binding)
        } else {
            None
        };
        let source_file = if matches!(
            call.name.as_str(),
            "patch_numbers_copy" | "replace_pages_copy_body"
        ) {
            let args: serde_json::Value =
                serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
            let target: ObjectRef =
                serde_json::from_value(args["target"].clone()).map_err(|_| unavailable())?;
            Some(super::text_file::directory_file_target(
                session,
                &target,
                now_unix_ms,
            )?)
        } else {
            None
        };
        let directories = crate::file_scope::approved_directories(session, now_unix_ms)
            .map_err(|_| unavailable())?;
        Self::build_with_batch_bindings(
            registry,
            surface,
            call,
            original,
            &directories,
            now_unix_ms,
            binding.as_ref(),
            word.as_ref(),
            excel.as_ref(),
            source_file.as_ref(),
        )
    }

    /// The optional binding is host-derived from an authenticated prior read.
    /// Presentation batch calls cannot use a File ref as their semantic target.
    pub fn build_with_presentation_binding(
        registry: &ProviderRegistry,
        surface: ProductSurface,
        call: &ToolCall,
        original: &ReadContextSelection,
        approved_directories: &[ObjectRef],
        now_unix_ms: u64,
        presentation: Option<&super::batch_document::PresentationReadBinding>,
    ) -> Result<Self, AgentError> {
        Self::build_with_batch_bindings(
            registry,
            surface,
            call,
            original,
            approved_directories,
            now_unix_ms,
            presentation,
            None,
            None,
            None,
        )
    }

    fn build_with_batch_bindings(
        registry: &ProviderRegistry,
        surface: ProductSurface,
        call: &ToolCall,
        original: &ReadContextSelection,
        approved_directories: &[ObjectRef],
        now_unix_ms: u64,
        presentation: Option<&super::batch_document::PresentationReadBinding>,
        word: Option<&super::word_read_binding::WordReadBinding>,
        excel: Option<&super::excel_read_binding::ExcelReadBinding>,
        source_file: Option<&ObjectRef>,
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
            if let Some(file) = source_file {
                return Ok(Some(file.clone()));
            }
            let files = selected_refs
                .iter()
                .filter(|reference| reference.object_kind == ObjectKind::File)
                .collect::<Vec<_>>();
            match files.as_slice() {
                [] => Ok(None),
                [file] => Ok(Some((*file).clone())),
                _ => Err(unavailable()),
            }
        };
        let validate_destination = |destination: &ObjectRef| {
            if destination.object_kind != ObjectKind::Directory
                || !approved_directories.contains(destination)
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
                let frozen = live_read::bound_target(original, "inspect_live_spreadsheet")?;
                let expiry = live_mutation_expiry(
                    original,
                    frozen,
                    &args.target,
                    ObjectKind::Range,
                    now_unix_ms,
                )?;
                (
                    args.target.clone(),
                    vec![args.target],
                    ComputerActionKind::SpreadsheetLive(args.action),
                    ComputerUseAdapterKind::IworkNumbers,
                    expiry,
                )
            }
            "replace_live_document_body" => {
                let args: DocumentActionArgs =
                    serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
                let frozen = live_read::bound_target(original, "inspect_live_document")?;
                let expiry = live_mutation_expiry(
                    original,
                    frozen,
                    &args.target,
                    ObjectKind::Document,
                    now_unix_ms,
                )?;
                (
                    args.target.clone(),
                    vec![args.target],
                    ComputerActionKind::DocumentLive(DocumentLivePatchAction::ReplaceBodyText {
                        text: args.text,
                    }),
                    ComputerUseAdapterKind::IworkPages,
                    expiry,
                )
            }
            "patch_live_presentation_slide" => {
                let args: PresentationActionArgs =
                    serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
                let frozen = live_read::bound_target(original, "inspect_live_presentation")?;
                let expiry = live_mutation_expiry(
                    original,
                    frozen,
                    &args.target,
                    ObjectKind::Slide,
                    now_unix_ms,
                )?;
                (
                    args.target.clone(),
                    vec![args.target],
                    ComputerActionKind::PresentationLive(args.action),
                    ComputerUseAdapterKind::IworkKeynote,
                    expiry,
                )
            }
            "patch_numbers_copy" => {
                let args: SpreadsheetBatchActionArgs =
                    serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
                if exact_batch_file()?.as_ref() != Some(&args.target) {
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
            "replace_pages_copy_body" => {
                let args: DocumentBatchActionArgs =
                    serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
                if exact_batch_file()?.as_ref() != Some(&args.target) {
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
            "patch_excel_copy" => {
                let args: SpreadsheetBatchActionArgs =
                    serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
                let binding = excel.ok_or_else(unavailable)?;
                if exact_batch_file()?.is_some_and(|file| file != binding.source().file)
                    || !desk_agent_protocol::computer_use::office_batch::is_xlsx(binding.adapter())
                {
                    return Err(unavailable());
                }
                binding.validate_target(&args.target, now_unix_ms)?;
                binding.validate_action(&args.action)?;
                validate_destination(&args.output.destination_parent)?;
                if !desk_agent_protocol::computer_use::office_batch::valid_xlsx_leaf(
                    &args.output.native_file_name,
                ) {
                    return Err(unavailable());
                }
                let refs = vec![args.target.clone(), args.output.destination_parent.clone()];
                (
                    args.target,
                    refs,
                    ComputerActionKind::SpreadsheetLiveBatch(SpreadsheetLiveBatchPatchAction {
                        output: args.output,
                        action: args.action,
                    }),
                    ComputerUseAdapterKind::OfficeExcel,
                    binding.valid_until_unix_ms(),
                )
            }
            "replace_word_copy_body" => {
                let args: DocumentBatchActionArgs =
                    serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
                let binding = word.ok_or_else(unavailable)?;
                if exact_batch_file()?.is_some_and(|file| file != binding.source().file)
                    || !desk_agent_protocol::computer_use::office_batch::is_docx(binding.adapter())
                {
                    return Err(unavailable());
                }
                binding.validate_target(&args.target, now_unix_ms)?;
                validate_destination(&args.output.destination_parent)?;
                let refs = vec![args.target.clone(), args.output.destination_parent.clone()];
                (
                    args.target,
                    refs,
                    ComputerActionKind::DocumentLiveBatch(DocumentLiveBatchPatchAction {
                        output: args.output,
                        action: DocumentLivePatchAction::ReplaceBodyText { text: args.text },
                    }),
                    ComputerUseAdapterKind::OfficeWord,
                    binding.valid_until_unix_ms(),
                )
            }
            "patch_keynote_copy" | "patch_powerpoint_copy" => {
                let args: PresentationBatchActionArgs =
                    serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
                let binding = presentation.ok_or_else(unavailable)?;
                let adapter = if call.name == "patch_powerpoint_copy" {
                    ComputerUseAdapterKind::OfficePowerPoint
                } else {
                    ComputerUseAdapterKind::IworkKeynote
                };
                if exact_batch_file()?.is_some_and(|file| file != binding.source().file)
                    || binding.adapter().kind != adapter
                {
                    return Err(unavailable());
                }
                binding.validate_target(&args.target, now_unix_ms)?;
                validate_destination(&args.output.destination_parent)?;
                let refs = vec![args.target.clone(), args.output.destination_parent.clone()];
                (
                    args.target,
                    refs,
                    ComputerActionKind::PresentationLiveBatch(PresentationLiveBatchPatchAction {
                        output: args.output,
                        action: args.action,
                    }),
                    adapter,
                    binding.valid_until_unix_ms(),
                )
            }
            _ => return Err(unavailable()),
        };
        if action.required_capability() != capability.required_capability {
            return Err(unavailable());
        }
        for reference in &authority_refs {
            if reference.object_kind == ObjectKind::Directory {
                continue;
            }
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
            input_revision: subject.input_revision,
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
