//! Independent file-side comparison with a trusted calculation result.
//! A cache match alone never proves that Excel recalculated or that publication succeeded.
use crate::{ooxml_package::PackageResult, xlsx_cells};
use desk_diagnose_core::spreadsheet_formula::{FORMULA_LOCALE_V1, validate_formula_patch};

#[derive(Clone, Debug, PartialEq)]
pub enum Scalar {
    Number(f64),
    Boolean(bool),
    Text(String),
}

#[derive(Debug, PartialEq)]
pub struct CacheMatch {
    pub sheet: String,
    pub address: String,
    pub formula_ast_digest_sha256: String,
    pub value: Scalar,
}

/// Expected value must originate from trusted calculation/readback, never from
/// model assertions. The caller still proves current-run completion, exact file
/// identity, source preservation and write-lease validity before issuing a receipt.
pub fn compare_formula_cache(
    bytes: &[u8],
    sheet: &str,
    address: &str,
    expected_ast_digest: &str,
    expected: Scalar,
) -> PackageResult<CacheMatch> {
    if matches!(&expected, Scalar::Number(number) if !number.is_finite()) {
        return Err("nonfinite expected calculation result".into());
    }
    let cell = xlsx_cells::inspect_stored(bytes, sheet, address, 8192)?
        .ok_or("calculated cell is absent")?;
    let formula = cell
        .formula
        .as_deref()
        .ok_or("calculated cell lost its formula")?;
    let validated = validate_formula_patch(
        &format!("={formula}"),
        address,
        FORMULA_LOCALE_V1,
        &[sheet.to_owned()],
    )?;
    if validated.ast_digest_sha256 != expected_ast_digest {
        return Err("calculated file formula differs from approved formula".into());
    }
    let cache = cell
        .value
        .as_deref()
        .ok_or("calculated file has no cached result")?;
    let actual = scalar(&cell.storage_type, cache)?;
    // Excel stores IEEE-754 doubles. Compare exactly, without a model-controlled
    // tolerance that could accept a stale or materially changed numeric result.
    if actual != expected {
        return Err("file cache differs from trusted calculation result".into());
    }
    Ok(CacheMatch {
        sheet: sheet.to_owned(),
        address: address.to_owned(),
        formula_ast_digest_sha256: validated.ast_digest_sha256,
        value: actual,
    })
}

pub(crate) fn scalar(kind: &str, text: &str) -> PackageResult<Scalar> {
    match kind {
        "n" => {
            if text.is_empty() || text.len() > 128 || text.trim() != text {
                return Err("invalid numeric formula cache".into());
            }
            let number: f64 = text.parse()?;
            if !number.is_finite() {
                return Err("nonfinite numeric formula cache".into());
            }
            Ok(Scalar::Number(number))
        }
        "b" => match text {
            "0" => Ok(Scalar::Boolean(false)),
            "1" => Ok(Scalar::Boolean(true)),
            _ => Err("invalid boolean formula cache".into()),
        },
        "e" => Err("calculated formula returned an Excel error".into()),
        "str" => Ok(Scalar::Text(crate::xlsx_strings::decode(text, 32 * 1024)?)),
        _ => Err("unsupported calculated result type".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    fn package(cell: &str) -> Vec<u8> {
        let mut parts = crate::xlsx_parts::tests::fixture();
        parts.insert("data/chosen.xml".into(), format!("<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData><row r=\"2\">{cell}</row></sheetData></worksheet>").into_bytes());
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, part) in parts {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(&part).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }
    fn digest(formula: &str) -> String {
        validate_formula_patch(formula, "A2", FORMULA_LOCALE_V1, &["数据 & report".into()])
            .unwrap()
            .ast_digest_sha256
    }
    #[test]
    fn compares_numeric_and_boolean_results_with_formula_identity() {
        let bytes = package("<c r=\"A2\"><f>21 * 2</f><v>4.2E1</v></c>");
        let before = bytes.clone();
        let matched = compare_formula_cache(
            &bytes,
            "数据 & report",
            "A2",
            &digest("=21*2"),
            Scalar::Number(42.0),
        )
        .unwrap();
        assert_eq!(matched.value, Scalar::Number(42.0));
        assert_eq!(matched.address, "A2");
        assert_eq!(bytes, before);
        assert!(
            compare_formula_cache(
                &bytes,
                "数据 & report",
                "A2",
                &digest("=20*2"),
                Scalar::Number(42.0)
            )
            .is_err()
        );
        assert!(
            compare_formula_cache(
                &bytes,
                "数据 & report",
                "A2",
                &digest("=21*2"),
                Scalar::Boolean(true)
            )
            .is_err()
        );
        let bytes = package("<c r=\"A2\" t=\"b\"><f>1&lt;2</f><v>1</v></c>");
        compare_formula_cache(
            &bytes,
            "数据 & report",
            "A2",
            &digest("=1<2"),
            Scalar::Boolean(true),
        )
        .unwrap();
    }
    #[test]
    fn rejects_missing_stale_changed_error_and_nonfinite_results() {
        for cell in [
            "<c r=\"A2\"><f>21*2</f></c>",
            "<c r=\"A2\"><f>21*2</f><v>24</v></c>",
            "<c r=\"A2\"><f>21*2</f><v>42.00000000000001</v></c>",
            "<c r=\"A2\"><v>42</v></c>",
            "<c r=\"B2\"><f>21*2</f><v>42</v></c>",
            "<c r=\"A2\" t=\"e\"><f>21*2</f><v>#VALUE!</v></c>",
            "<c r=\"A2\" t=\"str\"><f>21*2</f><v>42</v></c>",
            "<c r=\"A2\"><f>21*2</f><v>NaN</v></c>",
            "<c r=\"A2\"><f>21*2</f><v>1e999</v></c>",
            "<c r=\"A2\"><f>21*2</f><v>42</v><v>42</v></c>",
        ] {
            assert!(
                compare_formula_cache(
                    &package(cell),
                    "数据 & report",
                    "A2",
                    &digest("=21*2"),
                    Scalar::Number(42.0)
                )
                .is_err(),
                "{cell}"
            );
        }
        assert!(scalar("b", "true").is_err());
        assert!(scalar("n", " 42").is_err());
        assert!(
            compare_formula_cache(
                &package(""),
                "数据 & report",
                "A2",
                &digest("=21*2"),
                Scalar::Number(f64::NAN)
            )
            .is_err()
        );
    }
}
