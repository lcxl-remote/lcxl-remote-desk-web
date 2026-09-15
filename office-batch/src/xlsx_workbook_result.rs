//! Compare every formula cache with current-run native readback. This does not
//! prove native completion, preservation of nonformula content or publication.
use crate::{
    ooxml_package::{self, PackageResult},
    xlsx_cells, xlsx_formula_inventory, xlsx_parts,
    xlsx_result::{self, CacheMatch, Scalar},
};
use std::collections::BTreeMap;

/// Supplied by the native calculation boundary, never deserialized from model
/// arguments. The caller must bind these observations to its current invocation.
pub struct CalculationReadback {
    pub sheet: String,
    pub address: String,
    pub value: Scalar,
}

pub fn compare(
    prepared: &[u8],
    saved: &[u8],
    readback: &[CalculationReadback],
) -> PackageResult<Vec<CacheMatch>> {
    let before = xlsx_formula_inventory::inspect(prepared)?;
    let after = xlsx_formula_inventory::inspect(saved)?;
    if before.worksheet_count != after.worksheet_count
        || before.rule_formulas != after.rule_formulas
    {
        return Err("calculated workbook changed worksheet count or rule formulas".into());
    }
    let identities = |inventory: &xlsx_formula_inventory::Inventory| {
        inventory
            .formulas
            .iter()
            .map(|f| {
                (
                    (f.sheet.clone(), f.address.clone()),
                    f.ast_digest_sha256.clone(),
                )
            })
            .collect::<BTreeMap<_, _>>()
    };
    let expected = identities(&before);
    if expected != identities(&after) {
        return Err("calculated workbook changed formula identities".into());
    }
    if readback.len() != expected.len() {
        return Err("calculation readback does not cover all formulas".into());
    }
    let mut values = BTreeMap::new();
    for result in readback {
        let key = (result.sheet.clone(), result.address.clone());
        if !expected.contains_key(&key) || values.insert(key, result.value.clone()).is_some() {
            return Err("duplicate or unexpected calculation readback".into());
        }
    }
    // Expand each archive once here, not once per formula. Inspection above
    // independently enforces the all-sheet formula and package budgets.
    let original_parts = ooxml_package::read(prepared)?;
    let parts = ooxml_package::read(saved)?;
    let sheets = xlsx_parts::sheet_names(&parts)?;
    if sheets != xlsx_parts::sheet_names(&original_parts)? {
        return Err("calculated workbook changed worksheet identities or order".into());
    }
    let mut worksheets = BTreeMap::new();
    for sheet in sheets {
        let selected = xlsx_parts::locate(&parts, &sheet)?;
        worksheets.insert(sheet, selected.worksheet);
    }
    let mut matches = Vec::with_capacity(expected.len());
    for ((sheet, address), digest) in expected {
        let path = worksheets
            .get(&sheet)
            .ok_or("missing calculated worksheet")?;
        let cell = xlsx_cells::read_stored(&parts[path], &address, 65536)?
            .ok_or("missing calculated formula cell")?;
        let actual = xlsx_result::scalar(
            &cell.storage_type,
            cell.value
                .as_deref()
                .ok_or("missing calculated formula cache")?,
        )?;
        if actual != values[&(sheet.clone(), address.clone())] {
            return Err("file cache differs from current calculation readback".into());
        }
        matches.push(CacheMatch {
            sheet,
            address,
            formula_ast_digest_sha256: digest,
            value: actual,
        });
    }
    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    fn package(second_formula: &str, second_cache: &str) -> Vec<u8> {
        let mut parts = crate::xlsx_parts::tests::fixture();
        parts.insert("data/chosen.xml".into(), format!(
            "<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData><row r=\"2\"><c r=\"A2\"><f>21*2</f><v>42</v></c><c r=\"B2\"><f>{second_formula}</f>{second_cache}</c></row></sheetData></worksheet>"
        ).into_bytes());
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in parts {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(&data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }
    fn results() -> Vec<CalculationReadback> {
        [("B2", 43.0), ("A2", 42.0)]
            .into_iter()
            .map(|(address, value)| CalculationReadback {
                sheet: "数据 & report".into(),
                address: address.into(),
                value: Scalar::Number(value),
            })
            .collect()
    }
    #[test]
    fn verifies_all_formulas_independent_of_readback_order() {
        let prepared = package("A2+1", "");
        let saved = package("A2 + 1", "<v>43</v>");
        let matched = compare(&prepared, &saved, &results()).unwrap();
        assert_eq!(matched.len(), 2);
        assert_eq!(matched[1].address, "B2");
        assert_eq!(matched[1].value, Scalar::Number(43.0));
    }
    #[test]
    fn rejects_partial_duplicate_stale_and_changed_formula_results() {
        let prepared = package("A2+1", "");
        let saved = package("A2+1", "<v>43</v>");
        assert!(compare(&prepared, &saved, &results()[..1]).is_err());
        let mut duplicate = results();
        duplicate[0].address = "A2".into();
        assert!(compare(&prepared, &saved, &duplicate).is_err());
        let mut wrong = results();
        wrong[0].sheet = "other".into();
        assert!(compare(&prepared, &saved, &wrong).is_err());
        let mut nonfinite = results();
        nonfinite[0].value = Scalar::Number(f64::NAN);
        assert!(compare(&prepared, &saved, &nonfinite).is_err());
        for invalid in [
            package("A2+1", "<v>42</v>"),
            package("A2+1", ""),
            package("A2+2", "<v>43</v>"),
        ] {
            assert!(compare(&prepared, &invalid, &results()).is_err());
        }
    }
}
