//! Apply a formula only after revalidating the shared frozen formula policy.
//! The caller additionally binds file, sheet, target and policy into authorization.
use crate::{
    ooxml_package::PackageResult,
    xlsx_cells,
    xlsx_edit::{self, PendingRecalculation},
};
use desk_diagnose_core::spreadsheet_formula::{FORMULA_LOCALE_V1, validate_formula_patch};

/// The digest must come from the approved formula patch for this canonical A1
/// target. It is not authorization by itself. No cached result is written.
pub fn prepare(
    bytes: &[u8],
    sheet: &str,
    address: &str,
    formula: &str,
    expected_ast_digest: &str,
) -> PackageResult<PendingRecalculation> {
    xlsx_cells::validate_address(address)?;
    // Only the selected sheet is available to this operation. Cross-sheet data
    // requires its own selection and is not implicitly authorized by a formula.
    let validated =
        validate_formula_patch(formula, address, FORMULA_LOCALE_V1, &[sheet.to_owned()])?;
    if validated.ast_digest_sha256 != expected_ast_digest {
        return Err("spreadsheet formula digest does not match approved patch".into());
    }
    xlsx_edit::prepare_formula_text(
        bytes,
        sheet,
        address,
        formula
            .strip_prefix('=')
            .ok_or("formula requires equals prefix")?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ooxml_package;
    use std::io::{Cursor, Write};
    fn source() -> Vec<u8> {
        let mut parts = crate::xlsx_parts::tests::fixture();
        parts.insert("data/chosen.xml".into(), br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="2"><c r="A2" s="3"><f>1+1</f><v>2</v></c><c r="B2"><v>21</v></c></row></sheetData></worksheet>"#.to_vec());
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in parts {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(&data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }
    fn digest(formula: &str, target: &str) -> String {
        validate_formula_patch(
            formula,
            target,
            FORMULA_LOCALE_V1,
            &["数据 & report".into()],
        )
        .unwrap()
        .ast_digest_sha256
    }
    #[test]
    fn formula_copy_drops_old_cache_preserves_styles_and_reads_back_escaped_comparison() {
        let bytes = source();
        let before = ooxml_package::read(&bytes).unwrap();
        let formula = "=IF(B2<30,B2*2,0)";
        let output = prepare(
            &bytes,
            "数据 & report",
            "A2",
            formula,
            &digest(formula, "A2"),
        )
        .unwrap();
        let observed = xlsx_cells::inspect_stored(&output.bytes, "数据 & report", "A2", 100)
            .unwrap()
            .unwrap();
        assert_eq!(observed.formula.as_deref(), Some(&formula[1..]));
        assert_eq!(observed.value, None);
        let after = ooxml_package::read(&output.bytes).unwrap();
        let worksheet = std::str::from_utf8(&after["data/chosen.xml"]).unwrap();
        assert!(worksheet.contains("s=\"3\""));
        assert!(worksheet.contains("<c r=\"B2\"><v>21</v></c>"));
        assert!(worksheet.contains("B2&lt;30"));
        for (name, part) in &before {
            if name != "data/chosen.xml" {
                assert_eq!(&after[name], part);
            }
        }
        assert_eq!(ooxml_package::read(&bytes).unwrap(), before);
    }
    #[test]
    fn inserts_formula_into_missing_cells_and_rows_with_no_cached_result() {
        let bytes = source();
        for address in ["C2", "A1", "A9"] {
            let formula = "=B2*2";
            let output = prepare(
                &bytes,
                "数据 & report",
                address,
                formula,
                &digest(formula, address),
            )
            .unwrap();
            let observed = xlsx_cells::inspect_stored(&output.bytes, "数据 & report", address, 100)
                .unwrap()
                .unwrap();
            assert_eq!(observed.formula.as_deref(), Some("B2*2"));
            assert_eq!(observed.value, None);
            assert_eq!(
                xlsx_cells::inspect_stored(&output.bytes, "数据 & report", "B2", 100)
                    .unwrap()
                    .unwrap()
                    .value
                    .as_deref(),
                Some("21")
            );
        }
    }
    #[test]
    fn refuses_changed_formula_target_foreign_sheet_or_unsafe_function() {
        let bytes = source();
        let expected = digest("=B2*2", "A2");
        assert!(prepare(&bytes, "数据 & report", "A2", "=B2*3", &expected).is_err());
        assert!(prepare(&bytes, "数据 & report", "B2", "=B2*2", &expected).is_err());
        for formula in [
            "=Other!B2",
            "=WEBSERVICE(1)",
            "=INDIRECT(1)",
            "='[book.xlsx]Sheet1'!A1",
            "=cmd|' /C calc'!A0",
            "=RAND()",
            "B2*2",
        ] {
            assert!(
                prepare(&bytes, "数据 & report", "A2", formula, &expected).is_err(),
                "{formula}"
            );
        }
        assert!(prepare(&bytes, "数据 & report", "A2", "=B2*2", "").is_err());
    }
}
