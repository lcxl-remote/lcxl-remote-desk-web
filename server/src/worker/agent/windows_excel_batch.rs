//! XLSX observations and pending edits bound to Windows file-store snapshots.
//! The broker owns authorization; no native activation or publication occurs here.
use super::file_reference_store::windows_batch::{self, Format, Snapshot};
use desk_agent_protocol::{
    AgentError, AgentErrorKind,
    computer_use::{BatchDocumentSourceProjection, ObjectRef},
};
use desk_office_batch::{
    xlsx_cells::{self, Observation},
    xlsx_edit::{self, PendingRecalculation, Value},
    xlsx_formula,
    xlsx_formula_inventory::{self, Inventory},
};
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub struct CellObservation {
    snapshot: Arc<Snapshot>,
    sheet: String,
    address: String,
    projection: Option<Observation>,
}

/// Formula checks and an immutable digest describe preparation only. This type
/// must not be promoted to a verified FileArtifact without current-run native
/// completion, independent calculation readback and publication verification.
pub struct PreparedSpreadsheet {
    source: Arc<Snapshot>,
    pending: PendingRecalculation,
    sha256: String,
    formulas: Inventory,
}
impl PreparedSpreadsheet {
    pub fn pin_source(&self) -> Result<windows_batch::PinnedSource, AgentError> {
        self.source.pin()
    }
    pub fn bytes(&self) -> &[u8] {
        &self.pending.bytes
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
    pub fn formulas(&self) -> &Inventory {
        &self.formulas
    }
    /// Native runner supplies current-invocation readback and saved bytes. A
    /// successful comparison covers formula caches only; it cannot substitute
    /// for native completion, source/lease checks or create-new publication.
    pub fn compare_calculated_formulas(
        &self,
        saved: &[u8],
        readback: &[desk_office_batch::xlsx_workbook_result::CalculationReadback],
    ) -> Result<Vec<desk_office_batch::xlsx_result::CacheMatch>, AgentError> {
        desk_office_batch::xlsx_workbook_result::compare(self.bytes(), saved, readback)
            .map_err(format_error)
    }
}

pub fn observe_cell(
    source: &ObjectRef,
    sheet: &str,
    address: &str,
    limit: usize,
) -> Result<CellObservation, AgentError> {
    let snapshot = windows_batch::capture(source, Format::Spreadsheet)?;
    let projection =
        xlsx_cells::inspect(snapshot.bytes(), sheet, address, limit).map_err(format_error)?;
    snapshot.revalidate()?;
    Ok(CellObservation {
        snapshot: Arc::new(snapshot),
        sheet: sheet.to_owned(),
        address: address.to_owned(),
        projection,
    })
}

impl CellObservation {
    pub fn source(&self) -> BatchDocumentSourceProjection {
        self.snapshot.projection()
    }
    pub fn sheet(&self) -> &str {
        &self.sheet
    }
    pub fn address(&self) -> &str {
        &self.address
    }
    pub fn projection(&self) -> Option<&Observation> {
        self.projection.as_ref()
    }

    pub fn prepare_value(&self, value: Value<'_>) -> Result<PreparedSpreadsheet, AgentError> {
        self.snapshot.revalidate()?;
        let pending =
            xlsx_edit::prepare_value(self.snapshot.bytes(), &self.sheet, &self.address, value)
                .map_err(format_error)?;
        self.finish_preparation(pending)
    }
    pub fn prepare_formula(
        &self,
        formula: &str,
        approved_ast_digest: &str,
    ) -> Result<PreparedSpreadsheet, AgentError> {
        self.snapshot.revalidate()?;
        let pending = xlsx_formula::prepare(
            self.snapshot.bytes(),
            &self.sheet,
            &self.address,
            formula,
            approved_ast_digest,
        )
        .map_err(format_error)?;
        self.finish_preparation(pending)
    }
    fn finish_preparation(
        &self,
        pending: PendingRecalculation,
    ) -> Result<PreparedSpreadsheet, AgentError> {
        let formulas = xlsx_formula_inventory::inspect(&pending.bytes).map_err(format_error)?;
        self.snapshot.revalidate()?;
        Ok(PreparedSpreadsheet {
            source: Arc::clone(&self.snapshot),
            sha256: format!("{:x}", Sha256::digest(&pending.bytes)),
            pending,
            formulas,
        })
    }
}

fn format_error(cause: impl std::fmt::Display) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: format!("Excel batch: {cause}"),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::file_reference_store;
    use super::*;
    use std::io::{Cursor, Write};
    pub(crate) fn fixture(formula: &str) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, text) in [
            ("[Content_Types].xml", "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/><Override PartName=\"/xl/data.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/></Types>".to_owned()),
            ("_rels/.rels", "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"main\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/></Relationships>".to_owned()),
            ("xl/workbook.xml", "<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets><sheet name=\"数据\" sheetId=\"1\" r:id=\"data\"/></sheets></workbook>".to_owned()),
            ("xl/_rels/workbook.xml.rels", "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"data\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"data.xml\"/></Relationships>".to_owned()),
            ("xl/data.xml", format!("<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData><row r=\"1\"><c r=\"A1\"><v>21</v></c><c r=\"B1\"><f>{formula}</f><v>42</v></c></row></sheetData></worksheet>")),
        ] { zip.start_file(name, zip::write::SimpleFileOptions::default()).unwrap(); zip.write_all(text.as_bytes()).unwrap(); }
        zip.finish().unwrap().into_inner()
    }
    #[test]
    fn prepared_excel_keeps_its_original_source_after_observation_is_dropped() {
        let _guard = file_reference_store::file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.xlsx");
        std::fs::write(&path, fixture("A1*2")).unwrap();
        let reference = file_reference_store::issue(&path).unwrap();
        let prepared = observe_cell(&reference, "数据", "A1", 1024)
            .unwrap()
            .prepare_value(Value::Number("22"))
            .unwrap();
        let pin = prepared.pin_source().unwrap();
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err());
        drop(pin);
        std::fs::write(&path, fixture("A1*3")).unwrap();
        assert!(prepared.pin_source().is_err());
    }

    #[test]
    fn windows_excel_snapshot_prepares_bound_value_and_formula_without_publication() {
        let _guard = file_reference_store::file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("源文件.xlsx");
        let original = fixture("A1*2");
        std::fs::write(&path, &original).unwrap();
        let reference = file_reference_store::issue(&path).unwrap();
        let observed = observe_cell(&reference, "数据", "A1", 1024).unwrap();
        assert_eq!(observed.source().file, reference);
        assert_eq!(observed.sheet(), "数据");
        assert_eq!(observed.address(), "A1");
        assert_eq!(
            observed.projection().unwrap().stored.value.as_deref(),
            Some("21")
        );
        let value = observed.prepare_value(Value::Number("22")).unwrap();
        assert_eq!(
            xlsx_cells::inspect_stored(value.bytes(), "数据", "A1", 1024)
                .unwrap()
                .unwrap()
                .value
                .as_deref(),
            Some("22")
        );
        assert_eq!(value.formulas().formulas.len(), 1);
        let readback = [
            desk_office_batch::xlsx_workbook_result::CalculationReadback {
                sheet: "数据".into(),
                address: "B1".into(),
                value: desk_office_batch::xlsx_result::Scalar::Number(44.0),
            },
        ];
        // Editing A1 leaves B1's old cache in the pending package. It cannot be
        // accepted as calculated even though the selected literal edit succeeded.
        assert!(
            value
                .compare_calculated_formulas(value.bytes(), &readback)
                .is_err()
        );
        let mut calculated = zip::ZipArchive::new(Cursor::new(value.bytes())).unwrap();
        let mut saved = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for index in 0..calculated.len() {
            use std::io::Read;
            let mut entry = calculated.by_index(index).unwrap();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            if entry.name() == "xl/data.xml" {
                bytes = String::from_utf8(bytes)
                    .unwrap()
                    .replace("<v>42</v>", "<v>44</v>")
                    .into_bytes();
            }
            saved
                .start_file(entry.name(), zip::write::SimpleFileOptions::default())
                .unwrap();
            saved.write_all(&bytes).unwrap();
        }
        let saved = saved.finish().unwrap().into_inner();
        assert_eq!(
            value
                .compare_calculated_formulas(&saved, &readback)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            value.sha256(),
            format!("{:x}", Sha256::digest(value.bytes()))
        );
        let missing = observe_cell(&reference, "数据", "C3", 1024).unwrap();
        assert!(missing.projection().is_none());
        let proof = desk_diagnose_core::spreadsheet_formula::validate_formula_patch(
            "=A1*3",
            "C3",
            desk_diagnose_core::spreadsheet_formula::FORMULA_LOCALE_V1,
            &["数据".into()],
        )
        .unwrap();
        let formula = missing
            .prepare_formula("=A1*3", &proof.ast_digest_sha256)
            .unwrap();
        let read = xlsx_cells::inspect_stored(formula.bytes(), "数据", "C3", 1024)
            .unwrap()
            .unwrap();
        assert_eq!(read.formula.as_deref(), Some("A1*3"));
        assert_eq!(read.value, None);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
        std::fs::write(&path, b"changed after observation").unwrap();
        assert!(observed.prepare_value(Value::Number("23")).is_err());
        assert!(
            missing
                .prepare_formula("=A1*3", &proof.ast_digest_sha256)
                .is_err()
        );
    }
    #[test]
    fn windows_excel_preparation_rejects_unsafe_other_formula_and_wrong_selection() {
        let _guard = file_reference_store::file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("input.xlsx");
        std::fs::write(&path, fixture("WEBSERVICE(1)")).unwrap();
        let reference = file_reference_store::issue(&path).unwrap();
        assert!(observe_cell(&reference, "Missing", "A1", 100).is_err());
        assert!(observe_cell(&reference, "数据", "A0", 100).is_err());
        let observed = observe_cell(&reference, "数据", "A1", 100).unwrap();
        assert!(observed.prepare_value(Value::Number("22")).is_err());
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    }
}
