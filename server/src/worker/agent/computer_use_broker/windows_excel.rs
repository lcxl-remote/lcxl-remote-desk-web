//! Excel range references bind file snapshots and explicit worksheet/cell selection.
//! Batch publication retains the writer lease through native calculation and output creation.
use super::super::windows_excel_batch::{self, CellObservation, PreparedSpreadsheet};
use super::*;
use desk_agent_protocol::computer_use::{
    LiveDocumentInspectOutput, LiveDocumentProjection, SpreadsheetBatchInspectParams,
    SpreadsheetLiveBatchPatchAction, SpreadsheetLivePatchAction, office_batch,
};
use desk_office_batch::xlsx_edit::Value;

pub(super) fn readiness(
    ceiling: &ComputerUseSettings,
    session_ready: bool,
    session_reason: Option<ComputerUseReadinessReason>,
) -> ComputerUseCapabilityReadiness {
    let mut report = super::windows_office::readiness(ceiling, session_ready, session_reason);
    report.capability = Capability::SpreadsheetLiveInspect;
    report.adapter = ComputerUseAdapterRef {
        kind: ComputerUseAdapterKind::OfficeExcel,
        version: office_batch::XLSX_ADAPTER_VERSION.into(),
    };
    report
}

pub(super) fn mutation_readiness(
    ceiling: &ComputerUseSettings,
    session_ready: bool,
    session_reason: Option<ComputerUseReadinessReason>,
) -> ComputerUseCapabilityReadiness {
    let mut report = readiness(ceiling, session_ready, session_reason);
    report.capability = Capability::SpreadsheetLivePatchConfirmed;
    if report.ready && !crate::windows_office_helper::available() {
        report.ready = false;
        report.reason = Some(ComputerUseReadinessReason::AdapterUnavailable);
    }
    report
}

pub(crate) struct ExcelObservation {
    pub range: ObjectRef,
    pub observation: CellObservation,
}

impl ComputerUseBroker {
    pub(crate) fn prepare_excel_copy(
        &self,
        target: &ObjectRef,
        action: &SpreadsheetLivePatchAction,
        ceiling: &ComputerUseSettings,
    ) -> Result<PreparedSpreadsheet, AgentError> {
        self.with_excel_target(target, ceiling, |observed| match action {
            // The shared string action is always literal text. Native numeric
            // coercion must not silently turn user text into a formula or date.
            SpreadsheetLivePatchAction::SetCellValue { value } => {
                observed.prepare_value(Value::Text(value))
            }
            SpreadsheetLivePatchAction::SetCellNumber { value } => {
                if !office_batch::valid_number_literal(value) {
                    return Err(excel_error("invalid finite Excel number"));
                }
                observed.prepare_value(Value::Number(value))
            }
            SpreadsheetLivePatchAction::SetCellBoolean { value } => {
                observed.prepare_value(Value::Boolean(*value))
            }
            SpreadsheetLivePatchAction::SetCellFormula { formula } => {
                let proof = desk_diagnose_core::spreadsheet_formula::validate_formula_patch(
                    formula,
                    observed.address(),
                    desk_diagnose_core::spreadsheet_formula::FORMULA_LOCALE_V1,
                    &[observed.sheet().to_owned()],
                )
                .map_err(|_| excel_error("Excel formula does not match the restricted policy"))?;
                observed.prepare_formula(formula, &proof.ast_digest_sha256)
            }
        })
    }

    pub(crate) fn publish_excel_copy_with_lease(
        &self,
        target: &ObjectRef,
        batch: &SpreadsheetLiveBatchPatchAction,
        ceiling: &ComputerUseSettings,
        generation: &str,
    ) -> Result<
        super::super::file_reference_store::CreatedTextArtifact,
        super::super::file_reference_store::windows_publish::PublishFailure,
    > {
        use super::super::file_reference_store::windows_publish::{self, PublishFailure};
        self.require_writer_lease(generation)
            .map_err(PublishFailure::NotCreated)?;
        if batch.output.destination_parent.object_kind != ObjectKind::Directory
            || !office_batch::valid_xlsx_leaf(&batch.output.native_file_name)
        {
            return Err(PublishFailure::NotCreated(excel_error(
                "Excel copy requires a selected directory and safe XLSX name",
            )));
        }
        let prepared = self
            .prepare_excel_copy(target, &batch.action, ceiling)
            .map_err(PublishFailure::NotCreated)?;
        let _source = prepared.pin_source().map_err(PublishFailure::NotCreated)?;
        let calculated = if !prepared.formulas().formulas.is_empty()
            || !prepared.formulas().rule_formulas.is_empty()
        {
            let (saved, readback) =
                crate::windows_office_helper::calculate(prepared.bytes(), || {
                    self.require_writer_lease(generation)
                        .map_err(|e| anyhow::anyhow!(e.message))?;
                    self.resolve_ref(target)
                        .map_err(|e| anyhow::anyhow!(e.message))?;
                    Ok(())
                })
                .map_err(|e| {
                    PublishFailure::NotCreated(excel_error(&format!("Excel calculation: {e}")))
                })?;
            prepared
                .compare_calculated_formulas(&saved, &readback)
                .map_err(PublishFailure::NotCreated)?;
            Some(saved)
        } else {
            None
        };
        self.require_writer_lease(generation)
            .map_err(PublishFailure::NotCreated)?;
        self.resolve_ref(target)
            .map_err(PublishFailure::NotCreated)?;
        let published = windows_publish::publish_xlsx(
            &batch.output.destination_parent,
            &batch.output.native_file_name,
            calculated.as_deref().unwrap_or_else(|| prepared.bytes()),
        )?;
        self.require_writer_lease(generation)
            .map_err(PublishFailure::OutcomeUnknown)?;
        Ok(published)
    }
    pub(crate) fn inspect_excel_batch(
        &self,
        params: &SpreadsheetBatchInspectParams,
        ceiling: &ComputerUseSettings,
    ) -> Result<LiveDocumentInspectOutput, AgentError> {
        params.validate_selection().map_err(excel_error)?;
        let file = params
            .file
            .as_ref()
            .ok_or_else(|| excel_error("Excel requires an owner-selected file"))?;
        let observed = self.inspect_excel_cell(
            file,
            &params.sheet_name,
            &params.address,
            params.max_bytes,
            ceiling,
        )?;
        let range = observed.range;
        let snapshot_id = range.snapshot_id.clone();
        let resolved = self.resolve_ref(&range)?;
        let desktop = observe_interactive_desktop()?;
        let incarnation = format!(
            "{}:{}",
            desktop.session_id,
            self.current_incarnation_nonce()
        );
        let document = self.issue_ref(
            &snapshot_id,
            &incarnation,
            ObjectKind::Document,
            resolved.clone(),
        )?;
        let worksheet =
            self.issue_ref(&snapshot_id, &incarnation, ObjectKind::Worksheet, resolved)?;
        let cell = observed.observation.projection();
        let formula = cell
            .and_then(|cell| cell.stored.formula.as_ref())
            .map(|f| format!("={f}"));
        // Preserve absence/type and literal text without presenting an old
        // formula cache as a freshly computed value. No native formatting claim.
        let value = serde_json::json!({
            "present": cell.is_some(),
            "storage_type": cell.map(|cell| cell.stored.storage_type.as_str()),
            "value": cell.filter(|cell| cell.stored.formula.is_none()).and_then(|cell| cell.stored.value.as_deref()),
            "text": cell.filter(|cell| cell.stored.formula.is_none()).and_then(|cell| cell.text.as_deref()),
        }).to_string();
        let output = LiveDocumentInspectOutput {
            snapshot_id,
            adapter: ComputerUseAdapterRef {
                kind: ComputerUseAdapterKind::OfficeExcel,
                version: office_batch::XLSX_ADAPTER_VERSION.into(),
            },
            projection: LiveDocumentProjection::Spreadsheet {
                document,
                worksheet,
                range: range.clone(),
                sheet_name: params.sheet_name.clone(),
                table_name: String::new(),
                address: params.address.clone(),
                value,
                formula,
                formatted_value: String::new(),
            },
            batch_source: Some(observed.observation.source()),
        };
        if serde_json::to_vec(&output)
            .map_err(|_| excel_error("cannot encode Excel observation"))?
            .len()
            > params.max_bytes as usize
        {
            return Err(error(
                AgentErrorKind::OutputLimitExceeded,
                "Excel observation exceeds output budget",
                false,
            ));
        }
        self.resolve_ref(&range)?;
        Ok(output)
    }
    pub(crate) fn inspect_excel_cell(
        &self,
        file: &ObjectRef,
        sheet: &str,
        address: &str,
        limit: u32,
        ceiling: &ComputerUseSettings,
    ) -> Result<ExcelObservation, AgentError> {
        ensure_excel_enabled(ceiling)?;
        if !(1024..=MAX_COMPUTER_USE_INSPECT_BYTES).contains(&limit) {
            return Err(excel_error(
                "Excel observation requires a bounded output budget",
            ));
        }
        let desktop = observe_interactive_desktop()?;
        let incarnation = format!(
            "{}:{}",
            desktop.session_id,
            self.current_incarnation_nonce()
        );
        let observation = windows_excel_batch::observe_cell(
            file,
            sheet,
            address,
            (limit as usize).min(64 * 1024),
        )?;
        let source = observation.source();
        let after = observe_interactive_desktop()?;
        if after.session_id != desktop.session_id
            || incarnation != format!("{}:{}", after.session_id, self.current_incarnation_nonce())
        {
            return Err(excel_error(
                "interactive session changed during Excel observation",
            ));
        }
        let snapshot = self.next_snapshot_id();
        let range = self.issue_ref(
            &snapshot,
            &incarnation,
            ObjectKind::Range,
            ResolvedObject::ExcelBatch {
                source_file: file.clone(),
                source_sha256: source.sha256,
                source_byte_len: source.byte_len,
                sheet_name: sheet.to_owned(),
                cell_address: address.to_owned(),
            },
        )?;
        Ok(ExcelObservation { range, observation })
    }

    /// The dispatcher separately owns exact-action admission and write-lease acquisition.
    #[cfg(test)]
    pub(crate) fn prepare_excel_value(
        &self,
        target: &ObjectRef,
        value: Value<'_>,
        ceiling: &ComputerUseSettings,
    ) -> Result<PreparedSpreadsheet, AgentError> {
        self.with_excel_target(target, ceiling, |observed| observed.prepare_value(value))
    }
    #[cfg(test)]
    pub(crate) fn prepare_excel_formula(
        &self,
        target: &ObjectRef,
        formula: &str,
        approved_digest: &str,
        ceiling: &ComputerUseSettings,
    ) -> Result<PreparedSpreadsheet, AgentError> {
        self.with_excel_target(target, ceiling, |observed| {
            observed.prepare_formula(formula, approved_digest)
        })
    }
    fn with_excel_target(
        &self,
        target: &ObjectRef,
        ceiling: &ComputerUseSettings,
        prepare: impl FnOnce(&CellObservation) -> Result<PreparedSpreadsheet, AgentError>,
    ) -> Result<PreparedSpreadsheet, AgentError> {
        ensure_excel_enabled(ceiling)?;
        if target.object_kind != ObjectKind::Range {
            return Err(excel_error(
                "Excel preparation requires an observed cell range",
            ));
        }
        let resolved = self.resolve_ref(target)?;
        let ResolvedObject::ExcelBatch {
            source_file,
            source_sha256,
            source_byte_len,
            sheet_name,
            cell_address,
        } = &resolved
        else {
            return Err(excel_error("Excel target is not a batch cell observation"));
        };
        let observed =
            windows_excel_batch::observe_cell(source_file, sheet_name, cell_address, 64 * 1024)?;
        let source = observed.source();
        if source.sha256 != *source_sha256 || source.byte_len != *source_byte_len {
            return Err(excel_error("selected Excel file changed after observation"));
        }
        let prepared = prepare(&observed)?;
        if self.resolve_ref(target)? != resolved {
            return Err(excel_error("Excel target changed during preparation"));
        }
        Ok(prepared)
    }
}

fn ensure_excel_enabled(ceiling: &ComputerUseSettings) -> Result<(), AgentError> {
    ensure_observation_enabled(ceiling)?;
    if !ceiling.office_semantic {
        return Err(excel_error("Office batch capability is disabled"));
    }
    Ok(())
}
fn excel_error(message: &str) -> AgentError {
    error(AgentErrorKind::PermissionDenied, message, false)
}

#[cfg(test)]
mod tests {
    use super::super::super::file_reference_store;
    use super::*;
    mod native;
    #[test]
    fn windows_excel_broker_binds_cell_and_rejects_source_policy_and_worker_changes() {
        let _guard = file_reference_store::file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("源.xlsx");
        let original = windows_excel_batch::tests::fixture("A1*2");
        std::fs::write(&path, &original).unwrap();
        let file = file_reference_store::issue(&path).unwrap();
        let broker = ComputerUseBroker::default();
        let ceiling = ComputerUseSettings {
            enabled: true,
            observe: true,
            office_semantic: true,
            ..Default::default()
        };
        let observed = broker
            .inspect_excel_cell(&file, "数据", "C3", 4096, &ceiling)
            .unwrap();
        assert_eq!(observed.range.object_kind, ObjectKind::Range);
        assert_eq!(observed.observation.address(), "C3");
        assert_eq!(observed.observation.source().file, file);
        let prepared = broker
            .prepare_excel_value(&observed.range, Value::Number("9"), &ceiling)
            .unwrap();
        assert_eq!(
            desk_office_batch::xlsx_cells::inspect_stored(prepared.bytes(), "数据", "C3", 1024)
                .unwrap()
                .unwrap()
                .value
                .as_deref(),
            Some("9")
        );
        let proof = desk_diagnose_core::spreadsheet_formula::validate_formula_patch(
            "=A1*3",
            "C3",
            desk_diagnose_core::spreadsheet_formula::FORMULA_LOCALE_V1,
            &["数据".into()],
        )
        .unwrap();
        broker
            .prepare_excel_formula(&observed.range, "=A1*3", &proof.ast_digest_sha256, &ceiling)
            .unwrap();
        assert!(
            broker
                .prepare_excel_formula(&observed.range, "=A1*4", &proof.ast_digest_sha256, &ceiling)
                .is_err()
        );
        assert!(
            broker
                .prepare_excel_value(&file, Value::Number("9"), &ceiling)
                .is_err()
        );
        let mut disabled = ceiling.clone();
        disabled.office_semantic = false;
        assert!(
            broker
                .prepare_excel_value(&observed.range, Value::Number("9"), &disabled)
                .is_err()
        );
        assert!(
            broker
                .inspect_excel_cell(&file, "数据", "C3", 4096, &disabled)
                .is_err()
        );
        std::fs::write(&path, b"changed source").unwrap();
        assert!(
            broker
                .prepare_excel_value(&observed.range, Value::Number("9"), &ceiling)
                .is_err()
        );
        std::fs::write(&path, &original).unwrap();
        broker.reset_worker_incarnation();
        assert!(
            broker
                .prepare_excel_value(&observed.range, Value::Number("9"), &ceiling)
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    }
}
