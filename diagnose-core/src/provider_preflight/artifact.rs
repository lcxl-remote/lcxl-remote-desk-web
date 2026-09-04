//! Closed create-new local artifact inputs shared by both orchestrators.

use super::*;
use desk_agent_protocol::{
    communication::LocalDraftDocument,
    computer_use::{FilePatchAction, WordReportWebSource},
};

pub const TEXT_ARTIFACT_MEDIA_TYPE: &str = "text/plain;charset=utf-8";
pub const XLSX_ARTIFACT_MEDIA_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
pub const DOCX_ARTIFACT_MEDIA_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document";

fn unavailable() -> AgentError {
    error(
        AgentErrorKind::PermissionDenied,
        "local artifact input or original directory selection is unavailable",
        false,
        true,
    )
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TextArgs {
    file_name: String,
    content_utf8: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SpreadsheetArgs {
    preview_id: String,
    file_name: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SpreadsheetFormulaArgs {
    preview_id: String,
    file_name: String,
    target_cell: String,
    formula: String,
    locale: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WordArgs {
    preview_id: String,
    file_name: String,
    title: String,
    #[serde(default)]
    web_search_call_id: Option<String>,
    #[serde(default)]
    web_sources: Vec<WordReportWebSource>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalDraftArgs {
    file_name: String,
    draft: LocalDraftDocument,
}

fn safe_leaf(value: &str, suffix: Option<&str>) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && !matches!(value, "." | "..")
        && !value.ends_with(['.', ' '])
        && !value
            .chars()
            .any(|character| character.is_control() || "\\/:*?\"<>|".contains(character))
        && suffix.is_none_or(|suffix| {
            value.len() > suffix.len() && value.to_ascii_lowercase().ends_with(suffix)
        })
}

fn bounded_text(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

fn preview_id(value: &str) -> bool {
    bounded_text(value, 128)
}

/// Parse and validate one typed create-new action. The optional Word Search id
/// is intentionally not copied to the worker action: the durable Agent loop
/// resolves it against the original Web result and freezes that envelope in
/// [`crate::action_result::ActionResultOrigin`] before dispatch.
pub fn artifact_action_from_call(call: &ToolCall) -> Result<FilePatchAction, AgentError> {
    match call.name.as_str() {
        "create_text_artifact_in_selected_directory" => {
            let args: TextArgs =
                serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
            if !safe_leaf(&args.file_name, None)
                || args.content_utf8.is_empty()
                || args.content_utf8.len() > 65_536
            {
                return Err(unavailable());
            }
            Ok(FilePatchAction::CreateTextArtifact {
                file_name: args.file_name,
                content_utf8: args.content_utf8,
            })
        }
        "create_workbook_from_merge_preview" => {
            let args: SpreadsheetArgs =
                serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
            if !preview_id(&args.preview_id) || !safe_leaf(&args.file_name, Some(".xlsx")) {
                return Err(unavailable());
            }
            Ok(FilePatchAction::CreateSpreadsheetArtifact {
                preview_id: args.preview_id,
                file_name: args.file_name,
            })
        }
        "create_formula_workbook_from_merge_preview" => {
            let args: SpreadsheetFormulaArgs =
                serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
            if !preview_id(&args.preview_id) || !safe_leaf(&args.file_name, Some(".xlsx")) {
                return Err(unavailable());
            }
            let validated = crate::spreadsheet_formula::validate_formula_patch(
                &args.formula,
                &args.target_cell,
                &args.locale,
                &["Merged".into(), "Statistics".into()],
            )
            .map_err(|_| unavailable())?;
            if !matches!(
                &validated.target,
                crate::spreadsheet_formula::FormulaExpr::Cell { reference }
                    if reference.sheet.as_deref() == Some("Merged")
            ) {
                return Err(unavailable());
            }
            Ok(FilePatchAction::CreateSpreadsheetFormulaArtifact {
                preview_id: args.preview_id,
                file_name: args.file_name,
                target_cell: args.target_cell,
                formula: args.formula,
                locale: args.locale,
                formula_policy_digest_sha256: validated.ast_digest_sha256,
            })
        }
        "create_word_report_from_merge_preview" => {
            let args: WordArgs =
                serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
            if !preview_id(&args.preview_id)
                || !safe_leaf(&args.file_name, Some(".docx"))
                || !bounded_text(&args.title, 160)
                || args.web_search_call_id.is_some() != !args.web_sources.is_empty()
                || args
                    .web_search_call_id
                    .as_ref()
                    .is_some_and(|id| !bounded_text(id, 128))
                || args.web_sources.len() > 8
                || args.web_sources.iter().any(|source| {
                    !bounded_text(&source.title, 240)
                        || source.url.len() > 2_048
                        || !source.url.starts_with("https://")
                        || source.url.chars().any(char::is_control)
                })
            {
                return Err(unavailable());
            }
            let unique = args
                .web_sources
                .iter()
                .map(|source| (&source.title, &source.url))
                .collect::<std::collections::HashSet<_>>();
            if unique.len() != args.web_sources.len() {
                return Err(unavailable());
            }
            Ok(FilePatchAction::CreateWordReportArtifact {
                preview_id: args.preview_id,
                file_name: args.file_name,
                title: args.title,
                web_sources: args.web_sources,
            })
        }
        "create_local_communication_draft" => {
            let args: LocalDraftArgs =
                serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
            if !safe_leaf(&args.file_name, Some(".draft.txt")) || args.draft.validate().is_err() {
                return Err(unavailable());
            }
            Ok(FilePatchAction::CreateLocalCommunicationDraftArtifact {
                file_name: args.file_name,
                draft: args.draft,
            })
        }
        _ => Err(unavailable()),
    }
}

pub struct ArtifactCallPreflight {
    target: ObjectRef,
    action: FilePatchAction,
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

impl ArtifactCallPreflight {
    pub fn supports(tool_name: &str) -> bool {
        matches!(
            tool_name,
            "create_text_artifact_in_selected_directory"
                | "create_workbook_from_merge_preview"
                | "create_formula_workbook_from_merge_preview"
                | "create_word_report_from_merge_preview"
                | "create_local_communication_draft"
        )
    }

    pub fn build(
        registry: &ProviderRegistry,
        surface: ProductSurface,
        call: &ToolCall,
        selected_objects: &[ObjectRef],
        now_unix_ms: u64,
    ) -> Result<Self, AgentError> {
        let capability = registry
            .capability_for_tool(&call.name)
            .ok_or_else(unavailable)?;
        let provider = registry
            .provider_for_capability(&capability.wire.capability_id)
            .ok_or_else(unavailable)?;
        let directories = selected_objects
            .iter()
            .filter(|reference| reference.object_kind == ObjectKind::Directory)
            .collect::<Vec<_>>();
        if !Self::supports(&call.name)
            || !matches!(
                surface,
                ProductSurface::OssPersonalOwner | ProductSurface::ManagerPersonalOwner
            )
            || !capability.wire.surfaces.contains(&surface)
            || capability.wire.authorization_hint.resources
                != [AuthorizationResourceKind::FreshObjectReference]
            || call.arguments_json.len() > capability.wire.limits.max_input_bytes as usize
            || directories.len() != 1
            || now_unix_ms == 0
        {
            return Err(unavailable());
        }
        let target = directories[0].clone();
        let expiry = chrono::DateTime::parse_from_rfc3339(&target.expires_at)
            .ok()
            .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
            .filter(|expiry| *expiry > now_unix_ms)
            .ok_or_else(unavailable)?;
        let action = artifact_action_from_call(call)?;
        let action = ComputerActionKind::File(action);
        if action.required_capability() != capability.required_capability {
            return Err(unavailable());
        }
        let ComputerActionKind::File(action) = action else {
            unreachable!()
        };
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
            resource_scope: fresh_object_resource_scope(std::slice::from_ref(&target)),
            target,
            action,
            capability: capability.clone(),
            provider_id: provider.wire.provider_id.clone(),
            surface,
            canonical_input_digest_sha256: format!(
                "{:x}",
                Sha256::digest(canonical_input_json.as_bytes())
            ),
            canonical_input_json,
            operation_scope,
            risk_tier: classify_provider_call(capability, call)?,
            valid_until_unix_ms: expiry,
        })
    }

    pub fn target(&self) -> &ObjectRef {
        &self.target
    }
    pub fn action(&self) -> &FilePatchAction {
        &self.action
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
    pub fn adapter_version(&self) -> &'static str {
        match &self.action {
            FilePatchAction::CreateTextArtifact { .. }
            | FilePatchAction::CreateLocalCommunicationDraftArtifact { .. } => {
                crate::device_assistant::FILE_ARTIFACT_ADAPTER_VERSION
            }
            FilePatchAction::CreateSpreadsheetArtifact { .. }
            | FilePatchAction::CreateSpreadsheetFormulaArtifact { .. }
            | FilePatchAction::CreateWordReportArtifact { .. } => {
                crate::device_assistant::SPREADSHEET_FILE_ADAPTER_VERSION
            }
            _ => unreachable!("artifact preflight only constructs create-new actions"),
        }
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
