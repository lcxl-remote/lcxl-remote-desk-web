//! All-sheet cell and ordinary rule-formula inspection, not native-open authorization.
//! Relationship/type coverage, rule scope and native capabilities need separate preflight.
use crate::{
    ooxml_package::{self, PackageResult},
    ooxml_relationships::attributes,
    xlsx_cells, xlsx_parts, xlsx_rule_formulas, xlsx_strings,
};
use desk_diagnose_core::spreadsheet_formula::{FORMULA_LOCALE_V1, validate_formula_patch};
use quick_xml::{NsReader, events::Event, name::ResolveResult};
use std::collections::HashSet;
const S: &[u8] = b"http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const MAX_SHEETS: usize = 128;
const MAX_CELLS: usize = 100_000;
const MAX_FORMULAS: usize = 4096;
const MAX_FORMULA_BYTES: usize = 256 * 1024;
pub use crate::xlsx_rule_formulas::RuleFormula;

#[derive(Debug, PartialEq, Eq)]
pub struct Formula {
    pub sheet: String,
    pub address: String,
    /// Stored expression without an equals prefix, not its cached result.
    pub expression: String,
    pub ast_digest_sha256: String,
}
#[derive(Debug, PartialEq, Eq)]
pub struct Inventory {
    pub worksheet_count: usize,
    pub worksheet_names: Vec<String>,
    pub cell_count: usize,
    pub formulas: Vec<Formula>,
    pub rule_formulas: Vec<RuleFormula>,
}

pub fn inspect(bytes: &[u8]) -> PackageResult<Inventory> {
    let parts = ooxml_package::read(bytes)?;
    let sheets = xlsx_parts::sheet_names(&parts)?;
    if sheets.len() > MAX_SHEETS {
        return Err("too many worksheets for formula inspection".into());
    }
    let mut inventory = Inventory {
        worksheet_count: sheets.len(),
        worksheet_names: sheets.clone(),
        cell_count: 0,
        formulas: Vec::new(),
        rule_formulas: xlsx_rule_formulas::inspect(&parts, &sheets)?,
    };
    let mut visited = HashSet::new();
    let mut formula_bytes = 0;
    for sheet in &sheets {
        let selection = xlsx_parts::locate(&parts, sheet)?;
        if !visited.insert(selection.worksheet.clone()) {
            return Err("multiple sheets alias the same worksheet part".into());
        }
        scan(
            &parts[&selection.worksheet],
            sheet,
            &sheets,
            &mut inventory,
            &mut formula_bytes,
        )?;
    }
    Ok(inventory)
}

fn scan(
    xml: &[u8],
    sheet: &str,
    sheets: &[String],
    inventory: &mut Inventory,
    formula_bytes: &mut usize,
) -> PackageResult<()> {
    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().expand_empty_elements = true;
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut root = false;
    let mut data = false;
    let mut cell: Option<String> = None;
    let mut cells = HashSet::new();
    let mut formula: Option<String> = None;
    let mut seen_formula = false;
    loop {
        let (ns, event) = reader.read_resolved_event()?;
        let spreadsheet = matches!(ns, ResolveResult::Bound(ref n) if n.as_ref() == S);
        match event {
            Event::Start(element) => {
                let local = element.local_name();
                if local.as_ref() == b"AlternateContent"
                    && matches!(ns, ResolveResult::Bound(ref n) if n.as_ref() == b"http://schemas.openxmlformats.org/markup-compatibility/2006")
                {
                    return Err("alternate formula representations are unsupported".into());
                }
                let name = if spreadsheet {
                    local.as_ref().to_vec()
                } else {
                    Vec::new()
                };
                if formula.is_some() {
                    return Err("nested markup in spreadsheet formula".into());
                }
                if stack.is_empty() {
                    if root || name != b"worksheet" {
                        return Err("invalid worksheet root".into());
                    }
                    root = true;
                }
                if name == b"sheetData" {
                    if stack.len() != 1 || data {
                        return Err("ambiguous worksheet data".into());
                    }
                    data = true;
                }
                if name == b"c" {
                    if stack.len() != 3 || stack[1] != b"sheetData" || stack[2] != b"row" {
                        return Err("misplaced worksheet cell".into());
                    }
                    let attrs = attributes(&reader, &element)?;
                    let address = attrs
                        .get("r")
                        .ok_or("formula inspection needs explicit cell addresses")?;
                    xlsx_cells::validate_address(address)?;
                    if !cells.insert(address.clone()) {
                        return Err("duplicate worksheet cell".into());
                    }
                    inventory.cell_count += 1;
                    if inventory.cell_count > MAX_CELLS {
                        return Err("formula inspection cell limit exceeded".into());
                    }
                    cell = Some(address.clone());
                    seen_formula = false;
                }
                if name == b"f" {
                    if stack.len() != 4 || stack[3] != b"c" || cell.is_none() || seen_formula {
                        return Err("ambiguous or misplaced cell formula".into());
                    }
                    let attrs = attributes(&reader, &element)?;
                    if attrs
                        .iter()
                        .any(|(name, value)| name != "t" || value != "normal")
                    {
                        return Err("formula inspection requires expanded ordinary formulas".into());
                    }
                    seen_formula = true;
                    formula = Some(String::new());
                }
                stack.push(name);
            }
            Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) if formula.is_some() => {
                let text = xlsx_strings::event_text(event)?;
                let formula = formula.as_mut().ok_or("missing formula text")?;
                formula.push_str(&text);
                if formula.len() >= 4096 {
                    return Err("stored formula exceeds frozen policy limit".into());
                }
            }
            Event::End(_) => {
                let name = stack.pop().ok_or("invalid worksheet nesting")?;
                if name == b"f" {
                    let expression = formula.take().ok_or("missing formula expression")?;
                    let address = cell.as_ref().ok_or("missing formula address")?;
                    let validated = validate_formula_patch(
                        &format!("={expression}"),
                        address,
                        FORMULA_LOCALE_V1,
                        sheets,
                    )?;
                    *formula_bytes += expression.len();
                    if inventory.formulas.len() >= MAX_FORMULAS
                        || *formula_bytes > MAX_FORMULA_BYTES
                    {
                        return Err("formula inventory exceeds limit".into());
                    }
                    inventory.formulas.push(Formula {
                        sheet: sheet.to_owned(),
                        address: address.clone(),
                        expression,
                        ast_digest_sha256: validated.ast_digest_sha256,
                    });
                } else if name == b"c" {
                    cell = None;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !root || !data || !stack.is_empty() {
        return Err("incomplete worksheet formula scan".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    fn package(second_formula: &str) -> Vec<u8> {
        package_rule(second_formula, "Hidden!A1")
    }
    fn package_rule(second_formula: &str, rule: &str) -> Vec<u8> {
        let mut parts = crate::xlsx_parts::tests::fixture();
        let workbook = String::from_utf8(parts["custom/book.xml"].clone()).unwrap().replace("</w:sheets>", "<w:sheet name=\"Hidden\" sheetId=\"8\" state=\"veryHidden\" link:id=\"hidden\"/></w:sheets>");
        let workbook = workbook.replace("</w:workbook>", &format!("<w:definedNames><w:definedName name=\"amount\">{}</w:definedName></w:definedNames></w:workbook>", quick_xml::escape::escape(rule)));
        parts.insert("custom/book.xml".into(), workbook.into_bytes());
        let rels = String::from_utf8(parts["custom/_rels/book.xml.rels"].clone()).unwrap().replace("</Relationships>", "<Relationship Id=\"hidden\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"../data/hidden.xml\"/></Relationships>");
        parts.insert("custom/_rels/book.xml.rels".into(), rels.into_bytes());
        let types = String::from_utf8(parts["[Content_Types].xml"].clone()).unwrap().replace("</Types>", "<Override PartName=\"/data/hidden.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/></Types>");
        parts.insert("[Content_Types].xml".into(), types.into_bytes());
        for (name, formula) in [
            ("data/chosen.xml", "Hidden!A1*2"),
            ("data/hidden.xml", second_formula),
        ] {
            parts.insert(name.into(), format!("<worksheet xmlns=\"{}\"><sheetData><row r=\"1\"><c r=\"A1\"><f>{}</f><v>999</v></c></row></sheetData></worksheet>", std::str::from_utf8(S).unwrap(), quick_xml::escape::escape(formula)).into_bytes());
        }
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, part) in parts {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(&part).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }
    #[test]
    fn scans_hidden_sheets_and_validates_cross_sheet_references_without_using_caches() {
        let bytes = package("1+1");
        let before = bytes.clone();
        let inventory = inspect(&bytes).unwrap();
        assert_eq!(inventory.worksheet_count, 2);
        assert_eq!(inventory.cell_count, 2);
        assert_eq!(inventory.formulas.len(), 2);
        assert_eq!(inventory.rule_formulas.len(), 1);
        assert_eq!(inventory.rule_formulas[0].part, "custom/book.xml");
        assert_eq!(inventory.rule_formulas[0].expression, "Hidden!A1");
        assert!(inspect(&package_rule("1+1", "WEBSERVICE(1)")).is_err());
        assert_eq!(inventory.formulas[1].sheet, "Hidden");
        assert_eq!(inventory.formulas[1].expression, "1+1");
        assert_eq!(inventory.formulas[1].ast_digest_sha256.len(), 64);
        assert_eq!(bytes, before);
        for formula in [
            "WEBSERVICE(1)",
            "Other!A1",
            "RAND()",
            "'http://example.invalid/a'!A1",
        ] {
            assert!(inspect(&package(formula)).is_err(), "{formula}");
        }
    }
    #[test]
    fn rejects_ambiguous_unexpanded_and_oversized_formulas() {
        for cells in [
            "<c r=\"A1\"><f>1</f><f>2</f></c>",
            "<c r=\"A1\"/><c r=\"A1\"/>",
            "<c r=\"A1\"><f t=\"shared\" si=\"0\"/></c>",
            "<c r=\"A1\"><f><x/></f></c>",
        ] {
            let xml = format!(
                "<worksheet xmlns=\"{}\"><sheetData><row>{cells}</row></sheetData></worksheet>",
                std::str::from_utf8(S).unwrap()
            );
            let mut inventory = Inventory {
                worksheet_count: 1,
                worksheet_names: vec!["Sheet1".into()],
                cell_count: 0,
                formulas: vec![],
                rule_formulas: vec![],
            };
            assert!(
                scan(
                    xml.as_bytes(),
                    "Sheet1",
                    &["Sheet1".into()],
                    &mut inventory,
                    &mut 0
                )
                .is_err()
            );
        }
        assert!(inspect(&package(&"1+".repeat(2048))).is_err());
    }
}
