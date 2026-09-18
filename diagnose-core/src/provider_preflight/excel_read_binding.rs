//! Authenticated XLSX range evidence used by both central hosts.
use desk_agent_protocol::{
    AgentError, AgentErrorKind,
    computer_use::{
        BatchDocumentSourceProjection, ComputerUseAdapterRef, LiveDocumentInspectOutput,
        LiveDocumentProjection, ObjectKind, ObjectRef, SpreadsheetLivePatchAction, office_batch,
    },
};
use sha2::{Digest, Sha256};

pub struct ExcelReadBinding {
    source: BatchDocumentSourceProjection,
    target: ObjectRef,
    adapter: ComputerUseAdapterRef,
    sheet: String,
    address: String,
    valid_until: u64,
}
impl ExcelReadBinding {
    fn from_authenticated_read(
        selected: &ObjectRef,
        worker: &str,
        output: &LiveDocumentInspectOutput,
        args: &crate::device_assistant::windows_excel::InspectArgs,
        now: u64,
    ) -> Result<Self, AgentError> {
        let source = output.batch_source.as_ref().ok_or_else(denied)?;
        let LiveDocumentProjection::Spreadsheet {
            document,
            worksheet,
            range,
            sheet_name,
            address,
            table_name,
            value,
            formula,
            formatted_value,
        } = &output.projection
        else {
            return Err(denied());
        };
        if !office_batch::is_xlsx(&output.adapter)
            || selected.object_kind != ObjectKind::File
            || source.file != *selected
            || source.byte_len == 0
            || source.byte_len > 16 * 1024 * 1024
            || source.sha256.len() != 64
            || !source.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || sheet_name != &args.sheet_name
            || address != &args.address
            || !table_name.is_empty()
            || !formatted_value.is_empty()
            || value.len() > 64 * 1024
            || formula.as_ref().is_some_and(|f| f.len() > 4096)
            || worker.is_empty()
            || worker.len() > 4096
            || !output
                .snapshot_id
                .strip_prefix(worker)
                .and_then(|s| s.strip_prefix(':'))
                .and_then(|s| s.parse::<u64>().ok())
                .is_some_and(|n| n > 0)
        {
            return Err(denied());
        }
        let mut valid_until = super::word_read_binding::expiry(selected, now)?;
        let mut tokens = std::collections::HashSet::new();
        tokens.insert(selected.token.as_str());
        for (reference, kind) in [
            (document, ObjectKind::Document),
            (worksheet, ObjectKind::Worksheet),
            (range, ObjectKind::Range),
        ] {
            if reference.object_kind != kind
                || reference.snapshot_id != output.snapshot_id
                || !tokens.insert(reference.token.as_str())
            {
                return Err(denied());
            }
            valid_until = valid_until.min(super::word_read_binding::expiry(reference, now)?);
        }
        Ok(Self {
            source: source.clone(),
            target: range.clone(),
            adapter: output.adapter.clone(),
            sheet: sheet_name.clone(),
            address: address.clone(),
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
    pub fn validate_action(&self, action: &SpreadsheetLivePatchAction) -> Result<(), AgentError> {
        match action {
            SpreadsheetLivePatchAction::SetCellNumber { value } => {
                if !office_batch::valid_number_literal(value) {
                    return Err(denied());
                }
            }
            SpreadsheetLivePatchAction::SetCellBoolean { .. } => {}
            SpreadsheetLivePatchAction::SetCellValue { value } => {
                if value.len() > 32 * 1024
                    || value
                        .chars()
                        .any(|c| c.is_control() && !matches!(c, '\t' | '\r' | '\n'))
                {
                    return Err(denied());
                }
            }
            SpreadsheetLivePatchAction::SetCellFormula { formula } => {
                crate::spreadsheet_formula::validate_formula_patch(
                    formula,
                    &self.address,
                    crate::spreadsheet_formula::FORMULA_LOCALE_V1,
                    &[self.sheet.clone()],
                )
                .map_err(|_| denied())?;
            }
        }
        Ok(())
    }
}

pub fn resolve_excel_read(
    session: &crate::session::PersistedAgentSession,
    selected: &ObjectRef,
    worker: &str,
    target: &ObjectRef,
    now: u64,
) -> Result<ExcelReadBinding, AgentError> {
    use crate::{chat::ChatRole, device_assistant::windows_excel};
    let mut found = None;
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
            .filter(|m| m.role == ChatRole::Assistant)
            .flat_map(|m| &m.tool_calls)
            .filter(|call| call.id == call_id);
        let Some(call) = calls.next() else {
            continue;
        };
        if calls.next().is_some() || call.name != windows_excel::INSPECT_TOOL {
            continue;
        }
        let Ok(normalized) =
            super::text_file::selection::without_selectors(&crate::chat::ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments_json: call.arguments_json.clone(),
            })
        else {
            continue;
        };
        let Ok(args) =
            serde_json::from_str::<windows_excel::InspectArgs>(&normalized.arguments_json)
        else {
            continue;
        };
        let Some(envelope) = &message.trusted_tool_result().data_envelope else {
            continue;
        };
        if envelope.validate().is_err()
            || crate::model_egress::envelope_expires_by(envelope, now)
            || envelope.provenance.source_tool_name != call.name
            || envelope.provenance.source_provider_id != windows_excel::PROVIDER_ID
            || envelope.digest_sha256
                != format!(
                    "{:x}",
                    Sha256::digest(message.trusted_tool_result().text.as_bytes())
                )
        {
            continue;
        }
        let Ok(desk_agent_protocol::OperationOutput::ReadContext(
            desk_agent_protocol::ReadContextOutput::SpreadsheetLiveInspect(output),
        )) = serde_json::from_str(&message.trusted_tool_result().text)
        else {
            continue;
        };
        let Ok(binding) =
            ExcelReadBinding::from_authenticated_read(selected, worker, &output, &args, now)
        else {
            continue;
        };
        if binding.validate_target(target, now).is_err() {
            continue;
        }
        if found.is_some() {
            return Err(denied());
        }
        found = Some(binding);
    }
    found.ok_or_else(denied)
}
fn denied() -> AgentError {
    AgentError {
        kind: AgentErrorKind::PermissionDenied,
        message:
            "Excel batch range is not bound to the selected file, worksheet and current worker"
                .into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}
