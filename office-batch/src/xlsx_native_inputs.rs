//! Native result admission: Excel must calculate the same values and formulas.
//! This does not authorize publication or allow Excel's formatting into output.
use crate::{
    ooxml_package::{self, PackageResult},
    ooxml_relationships as rel, xlsx_cells, xlsx_formula_inventory, xlsx_parts,
    xlsx_result::{self, Scalar},
    xlsx_strings,
};
use std::collections::BTreeMap;

#[derive(Debug, PartialEq)]
enum Value {
    Scalar(Scalar),
    Formula(String),
    Error(String),
    Date(String),
}

fn values(
    parts: &rel::Parts,
    inventory: &xlsx_formula_inventory::Inventory,
) -> PackageResult<BTreeMap<(String, String), Value>> {
    let formulas = inventory
        .formulas
        .iter()
        .map(|f| {
            (
                (f.sheet.as_str(), f.address.as_str()),
                f.ast_digest_sha256.as_str(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut strings = BTreeMap::new();
    let mut values = BTreeMap::new();
    let mut total_bytes = 0usize;
    for sheet in &inventory.worksheet_names {
        let selected = xlsx_parts::locate(parts, sheet)?;
        if let Some(path) = &selected.shared_strings {
            if !strings.contains_key(path) {
                strings.insert(path.clone(), xlsx_strings::shared_all(&parts[path], 65536)?);
            }
        }
        for (address, cell) in xlsx_cells::read_all(&parts[&selected.worksheet], 65536)? {
            let value = if cell.formula.is_some() {
                Value::Formula(
                    formulas
                        .get(&(sheet.as_str(), address.as_str()))
                        .ok_or("unvalidated native formula")?
                        .to_string(),
                )
            } else if let Some(text) = cell.value {
                match cell.storage_type.as_str() {
                    "n" | "b" | "str" => {
                        Value::Scalar(xlsx_result::scalar(&cell.storage_type, &text)?)
                    }
                    "inlineStr" => Value::Scalar(Scalar::Text(text)),
                    "s" => {
                        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
                            return Err("invalid native shared string index".into());
                        }
                        let path = selected
                            .shared_strings
                            .as_ref()
                            .ok_or("missing native shared strings")?;
                        Value::Scalar(Scalar::Text(
                            strings[path]
                                .get(text.parse::<usize>()?)
                                .ok_or("native shared string index absent")?
                                .clone(),
                        ))
                    }
                    "e" => Value::Error(text),
                    "d" => Value::Date(text),
                    _ => return Err("unsupported native input value".into()),
                }
            } else {
                continue;
            };
            let value_bytes = match &value {
                Value::Scalar(Scalar::Text(text))
                | Value::Formula(text)
                | Value::Error(text)
                | Value::Date(text) => text.len(),
                _ => 16,
            };
            total_bytes = total_bytes
                .checked_add(128 + sheet.len() + address.len() + value_bytes)
                .ok_or("native input inventory overflow")?;
            if total_bytes > 32 * 1024 * 1024 {
                return Err("native input inventory exceeds bound".into());
            }
            values.insert((sheet.clone(), address), value);
        }
    }
    Ok(values)
}

pub fn validate(prepared: &[u8], saved: &[u8]) -> PackageResult<()> {
    let before = ooxml_package::read(prepared)?;
    let after = ooxml_package::read(saved)?;
    let original = xlsx_formula_inventory::inspect(prepared)?;
    let native = xlsx_formula_inventory::inspect(saved)?;
    if calculation_profile(&before)? != calculation_profile(&after)? {
        return Err("native calculation changed the date system".into());
    }
    if original.worksheet_names != native.worksheet_names
        || original.rule_formulas != native.rule_formulas
    {
        return Err("native calculation changed worksheets or rule formulas".into());
    }
    if values(&before, &original)? != values(&after, &native)? {
        return Err("native calculation changed an input value or formula".into());
    }
    Ok(())
}

fn calculation_profile(parts: &rel::Parts) -> PackageResult<bool> {
    use quick_xml::{NsReader, events::Event, name::ResolveResult};
    let main = rel::main_part(parts)?;
    let mut reader = NsReader::from_reader(parts[&main].as_slice());
    reader.config_mut().expand_empty_elements = true;
    let mut date1904 = false;
    let mut seen = std::collections::HashSet::new();
    loop {
        let (ns, event) = reader.read_resolved_event()?;
        match event {
            Event::Start(element) if matches!(ns, ResolveResult::Bound(ref ns) if ns.as_ref() == b"http://schemas.openxmlformats.org/spreadsheetml/2006/main") =>
            {
                let local = element.local_name();
                if !matches!(local.as_ref(), b"workbookPr" | b"calcPr") {
                    continue;
                }
                if !seen.insert(local.as_ref().to_vec()) {
                    return Err("duplicate calculation properties".into());
                }
                let attrs = rel::attributes(&reader, &element)?;
                let boolean = |key: &str, default: bool| -> PackageResult<bool> {
                    match attrs.get(key).map(String::as_str) {
                        None => Ok(default),
                        Some("0" | "false") => Ok(false),
                        Some("1" | "true") => Ok(true),
                        _ => Err("invalid calculation property".into()),
                    }
                };
                if local.as_ref() == b"workbookPr" {
                    date1904 = boolean("date1904", false)?;
                } else if !boolean("fullPrecision", true)? || boolean("iterate", false)? {
                    return Err("display-precision or iterative calculation is unsupported".into());
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(date1904)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    fn package(number: &str, formula: &str, cache: &str) -> Vec<u8> {
        let mut parts = crate::xlsx_parts::tests::fixture();
        parts.insert("data/chosen.xml".into(), format!("<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData><row r=\"2\"><c r=\"A2\"><v>{number}</v></c><c r=\"B2\"><f>{formula}</f><v>{cache}</v></c></row></sheetData></worksheet>").into_bytes());
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in parts {
            zip.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(&bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }
    #[test]
    fn input_or_formula_changes_fail_but_numeric_spelling_and_cache_changes_do_not() {
        let original = package("21", "A2*2", "0");
        validate(&original, &package("21.0", "A2*2", "42")).unwrap();
        assert!(validate(&original, &package("22", "A2*2", "44")).is_err());
        assert!(validate(&original, &package("21", "A2*3", "63")).is_err());
    }
    #[test]
    fn all_cells_reader_rejects_duplicate_inputs() {
        let xml = b"<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData><row r=\"1\"><c r=\"A1\"><v>1</v></c><c r=\"A1\"><v>2</v></c></row></sheetData></worksheet>";
        assert!(xlsx_cells::read_all(xml, 65536).is_err());
    }
}
