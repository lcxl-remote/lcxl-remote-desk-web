//! Selected stored cell data. Formula caches are not recalculation results.
//! Shared-string indices remain indices and must be resolved before user display.
use crate::{
    ooxml_package::{self, PackageResult},
    ooxml_relationships::attributes,
    xlsx_parts, xlsx_strings,
};
use quick_xml::{NsReader, events::Event, name::ResolveResult};
const S: &[u8] = b"http://schemas.openxmlformats.org/spreadsheetml/2006/main";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct StoredCell {
    /// OOXML storage type, e.g. n, b, s, str, e, d, inlineStr.
    pub storage_type: String,
    pub value: Option<String>,
    pub formula: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Observation {
    pub stored: StoredCell,
    /// Resolved string content only; numbers and dates retain their raw storage.
    pub text: Option<String>,
}

pub fn inspect(
    bytes: &[u8],
    sheet_name: &str,
    address: &str,
    limit: usize,
) -> PackageResult<Option<Observation>> {
    validate_address(address)?;
    let parts = ooxml_package::read(bytes)?;
    let selected = xlsx_parts::locate(&parts, sheet_name)?;
    let Some(stored) = read_stored(&parts[&selected.worksheet], address, limit)? else {
        return Ok(None);
    };
    let remaining = limit.saturating_sub(stored.formula.as_ref().map_or(0, String::len));
    let text = match stored.storage_type.as_str() {
        "s" => {
            let index = stored
                .value
                .as_deref()
                .ok_or("missing shared string index")?;
            if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
                return Err("invalid shared string index".into());
            }
            let path = selected
                .shared_strings
                .as_ref()
                .ok_or("missing shared strings relationship")?;
            Some(xlsx_strings::shared(
                &parts[path],
                index.parse()?,
                remaining,
            )?)
        }
        "inlineStr" => stored.value.clone(),
        "str" => stored
            .value
            .as_ref()
            .map(|s| xlsx_strings::decode(s, remaining))
            .transpose()?,
        _ => None,
    };
    Ok(Some(Observation { stored, text }))
}

/// Inspect one cell of the exact named worksheet within a bounded XLSX package.
/// This is raw storage evidence, not formatted text or evaluated formula output.
pub fn inspect_stored(
    bytes: &[u8],
    sheet_name: &str,
    address: &str,
    limit: usize,
) -> PackageResult<Option<StoredCell>> {
    validate_address(address)?;
    let parts = ooxml_package::read(bytes)?;
    let selected = xlsx_parts::locate(&parts, sheet_name)?;
    read_stored(&parts[&selected.worksheet], address, limit)
}

/// Strict canonical A1 identity within Excel's row and column bounds.
pub fn validate_address(address: &str) -> PackageResult<()> {
    let split = address.bytes().take_while(u8::is_ascii_uppercase).count();
    if split == 0 || split > 3 || split == address.len() {
        return Err("invalid cell address".into());
    }
    let (column, row) = address.split_at(split);
    let column = column
        .bytes()
        .fold(0u32, |n, b| n * 26 + u32::from(b - b'A' + 1));
    if row.starts_with('0') || !row.bytes().all(|b| b.is_ascii_digit()) {
        return Err("invalid cell row".into());
    }
    let row: u32 = row.parse()?;
    if column > 16_384 || row > 1_048_576 {
        return Err("cell address exceeds worksheet bounds".into());
    }
    Ok(())
}

/// Called only after `ooxml_package::read` and `xlsx_parts::locate`.
/// None denotes an absent cell; an explicit empty cell is Some with no value.
/// Shared-string indices are unresolved; inline strings contain decoded text.
/// No native application is opened by this function.
pub(crate) fn read_stored(
    xml: &[u8],
    address: &str,
    limit: usize,
) -> PackageResult<Option<StoredCell>> {
    validate_address(address)?;
    Ok(read_cells(xml, Some(address), limit)?.remove(address))
}

/// One pass over a worksheet, for checking all native calculation inputs.
pub(crate) fn read_all(
    xml: &[u8],
    limit: usize,
) -> PackageResult<std::collections::BTreeMap<String, StoredCell>> {
    read_cells(xml, None, limit)
}

fn read_cells(
    xml: &[u8],
    requested: Option<&str>,
    limit: usize,
) -> PackageResult<std::collections::BTreeMap<String, StoredCell>> {
    if limit == 0 || limit > 65_536 {
        return Err("invalid cell observation limit".into());
    }
    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().expand_empty_elements = true;
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut result = None;
    let mut cells = std::collections::BTreeMap::new();
    let mut selected_address = None;
    let mut selected = false;
    let mut seen_root = false;
    let mut seen_data = false;
    let mut inline: Option<xlsx_strings::RichText> = None;
    loop {
        let (ns, event) = reader.read_resolved_event()?;
        let spreadsheet = matches!(ns, ResolveResult::Bound(ref n) if n.as_ref() == S);
        match event {
            Event::Start(element) => {
                let name = if spreadsheet {
                    element.local_name().as_ref().to_vec()
                } else {
                    Vec::new()
                };
                if stack.is_empty() {
                    if seen_root || name != b"worksheet" {
                        return Err("invalid worksheet root".into());
                    }
                    seen_root = true;
                }
                if name == b"sheetData" {
                    if stack.len() != 1 || seen_data {
                        return Err("invalid worksheet data structure".into());
                    }
                    seen_data = true;
                }
                if name == b"c" {
                    let attrs = attributes(&reader, &element)?;
                    if requested
                        .is_none_or(|address| attrs.get("r").map(String::as_str) == Some(address))
                    {
                        let address = attrs.get("r").ok_or("cell address is missing")?;
                        validate_address(address)?;
                        if result.is_some()
                            || cells.contains_key(address)
                            || stack.as_slice()
                                != [
                                    b"worksheet".to_vec(),
                                    b"sheetData".to_vec(),
                                    b"row".to_vec(),
                                ]
                        {
                            return Err("duplicate or misplaced selected cell".into());
                        }
                        let storage_type = attrs.get("t").cloned().unwrap_or_else(|| "n".into());
                        if !matches!(
                            storage_type.as_str(),
                            "n" | "b" | "s" | "str" | "e" | "d" | "inlineStr"
                        ) {
                            return Err("unsupported selected cell storage type".into());
                        }
                        result = Some(StoredCell {
                            storage_type,
                            ..Default::default()
                        });
                        selected = true;
                        selected_address = Some(address.clone());
                    } else if selected {
                        return Err("nested selected cell".into());
                    }
                } else if selected {
                    if let Some(value) = &mut inline {
                        value.start(&name)?;
                        stack.push(name);
                        continue;
                    }
                    let cell = result.as_mut().ok_or("missing selected cell")?;
                    if cell.storage_type == "inlineStr" {
                        if name != b"is" || stack.len() != 4 || cell.value.is_some() {
                            return Err("invalid inline string cell".into());
                        }
                        inline = Some(xlsx_strings::RichText::new(limit));
                        stack.push(name);
                        continue;
                    }
                    if stack.len() != 4 || !matches!(name.as_slice(), b"v" | b"f") {
                        return Err("unsupported selected cell content".into());
                    }
                    let cell = result.as_mut().ok_or("missing selected cell")?;
                    let slot = if name == b"f" {
                        let attrs = attributes(&reader, &element)?;
                        if attrs.iter().any(|(k, v)| k != "t" || v != "normal") {
                            return Err(
                                "non-normal or attributed cell formula is unsupported".into()
                            );
                        }
                        &mut cell.formula
                    } else {
                        &mut cell.value
                    };
                    if slot.replace(String::new()).is_some() {
                        return Err("duplicate cell value or formula".into());
                    }
                }
                stack.push(name);
            }
            Event::End(_) => {
                if inline.is_some() {
                    if stack.len() == 5 {
                        let text = inline.take().ok_or("missing inline string")?.finish()?;
                        result.as_mut().ok_or("missing selected cell")?.value = Some(text);
                    } else {
                        inline.as_mut().ok_or("missing inline string")?.end()?;
                    }
                }
                if selected && stack.len() == 4 {
                    cells.insert(
                        selected_address
                            .take()
                            .ok_or("selected cell address missing")?,
                        result.take().ok_or("selected cell content missing")?,
                    );
                    selected = false;
                }
                stack.pop().ok_or("invalid worksheet nesting")?;
            }
            Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) if selected => {
                let text = xlsx_strings::event_text(event)?;
                if let Some(value) = &mut inline {
                    value.text(&text)?;
                    continue;
                }
                let cell = result.as_mut().ok_or("missing selected cell")?;
                let slot = match stack.last().map(Vec::as_slice) {
                    Some(b"v") => &mut cell.value,
                    Some(b"f") => &mut cell.formula,
                    _ if text.trim().is_empty() => continue,
                    _ => return Err("text outside selected cell value".into()),
                };
                slot.as_mut()
                    .ok_or("missing cell text slot")?
                    .push_str(&text);
                if cell.value.as_ref().map_or(0, String::len)
                    + cell.formula.as_ref().map_or(0, String::len)
                    > limit
                {
                    return Err("cell observation exceeds limit".into());
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !seen_root || !seen_data || !stack.is_empty() {
        return Err("incomplete worksheet".into());
    }
    Ok(cells)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn package(parts: &std::collections::BTreeMap<String, Vec<u8>>) -> Vec<u8> {
        use std::io::{Cursor, Write};
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in parts {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }
    fn sheet(cells: &str) -> String {
        format!(
            "<worksheet xmlns=\"{}\"><sheetData><row r=\"2\">{cells}</row></sheetData></worksheet>",
            std::str::from_utf8(S).unwrap()
        )
    }
    #[test]
    fn selected_text_decodes_entities_with_namespace_identity_and_byte_limit() {
        let xml = sheet("<c r=\"A2\" t=\"str\"><v>中&amp;&#60;<![CDATA[>]]></v></c>");
        let cell = read_stored(xml.as_bytes(), "A2", 6).unwrap().unwrap();
        assert_eq!(cell.value.as_deref(), Some("中&<>"));
        assert!(read_stored(xml.as_bytes(), "A2", 5).is_err());
        let alias = xml
            .replace("xmlns=", "xmlns:s=")
            .replace("<worksheet", "<s:worksheet")
            .replace("</worksheet", "</s:worksheet");
        // Unqualified sheetData must not be mistaken for spreadsheet elements.
        assert!(read_stored(alias.as_bytes(), "A2", 100).is_err());
        let spoof =
            sheet("<c xmlns=\"urn:foreign\" r=\"A2\"><v>wrong</v></c><c r=\"A2\"><v>7</v></c>");
        assert_eq!(
            read_stored(spoof.as_bytes(), "A2", 100)
                .unwrap()
                .unwrap()
                .value
                .as_deref(),
            Some("7")
        );
    }
    #[test]
    fn package_inspection_selects_custom_worksheet_and_rejects_unsafe_package() {
        let mut parts = crate::xlsx_parts::tests::fixture();
        parts.insert(
            "data/chosen.xml".into(),
            sheet("<c r=\"A2\"><v>42</v></c>").into_bytes(),
        );
        let bytes = package(&parts);
        let before = bytes.clone();
        assert_eq!(
            inspect_stored(&bytes, "数据 & report", "A2", 100)
                .unwrap()
                .unwrap()
                .value
                .as_deref(),
            Some("42")
        );
        assert!(inspect_stored(&bytes, "Sheet1", "A2", 100).is_err());
        assert_eq!(bytes, before);
        parts.insert("../escape.xml".into(), b"<x/>".to_vec());
        assert!(inspect_stored(&package(&parts), "数据 & report", "A2", 100).is_err());
    }
    #[test]
    fn observes_inline_text_without_double_decoding_and_rejects_mixed_storage() {
        let mut parts = crate::xlsx_parts::tests::fixture();
        parts.insert("data/chosen.xml".into(), sheet("<c r=\"A2\" t=\"inlineStr\"><is><r><t>_x005F_x0041_</t></r><r><t>中</t></r><rPh><t>ignore</t></rPh></is></c>").into_bytes());
        let observation = inspect(&package(&parts), "数据 & report", "A2", 10)
            .unwrap()
            .unwrap();
        assert_eq!(observation.text.as_deref(), Some("_x0041_中"));
        assert_eq!(observation.stored.storage_type, "inlineStr");
        assert!(inspect(&package(&parts), "数据 & report", "A2", 9).is_err());
        for cell in [
            "<c r=\"A2\" t=\"inlineStr\"><is/><is/></c>",
            "<c r=\"A2\" t=\"inlineStr\"><is/><f>1+1</f></c>",
            "<c r=\"A2\"><is/></c>",
        ] {
            assert!(read_stored(sheet(cell).as_bytes(), "A2", 100).is_err());
        }
    }
    #[test]
    fn resolves_shared_text_only_through_the_validated_relationship() {
        let mut parts = crate::xlsx_parts::tests::fixture();
        let set_cell = |parts: &mut std::collections::BTreeMap<String, Vec<u8>>, index: &str| {
            parts.insert(
                "data/chosen.xml".into(),
                sheet(&format!("<c r=\"A2\" t=\"s\"><v>{index}</v></c>")).into_bytes(),
            );
        };
        set_cell(&mut parts, "1");
        assert!(inspect(&package(&parts), "数据 & report", "A2", 100).is_err());
        let rels = String::from_utf8(parts["custom/_rels/book.xml.rels"].clone()).unwrap().replace("</Relationships>", "<Relationship Id=\"strings\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings\" Target=\"../data/strings.xml\"/></Relationships>");
        parts.insert("custom/_rels/book.xml.rels".into(), rels.into_bytes());
        let types = String::from_utf8(parts["[Content_Types].xml"].clone()).unwrap().replace("</Types>", "<Override PartName=\"/data/strings.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml\"/></Types>");
        parts.insert("[Content_Types].xml".into(), types.into_bytes());
        parts.insert(
            "data/strings.xml".into(),
            format!(
                "<sst xmlns=\"{}\"><si><t>other</t></si><si><t>选中</t></si></sst>",
                std::str::from_utf8(S).unwrap()
            )
            .into_bytes(),
        );
        let bytes = package(&parts);
        let observation = inspect(&bytes, "数据 & report", "A2", 6).unwrap().unwrap();
        assert_eq!(observation.stored.value.as_deref(), Some("1"));
        assert_eq!(observation.text.as_deref(), Some("选中"));
        assert!(inspect(&bytes, "数据 & report", "A2", 5).is_err());
        for invalid in ["2", "-1", "1.0", "", "999999999999999999999999"] {
            set_cell(&mut parts, invalid);
            assert!(
                inspect(&package(&parts), "数据 & report", "A2", 100).is_err(),
                "{invalid}"
            );
        }
    }
    #[test]
    fn stored_formula_cache_is_separate_and_selection_is_exact() {
        let xml =
            sheet("<c r=\"A2\"><f>SUM(B2,C2)</f><v>42</v></c><c r=\"B2\" t=\"s\"><v>3</v></c>");
        assert_eq!(
            read_stored(xml.as_bytes(), "A2", 100).unwrap(),
            Some(StoredCell {
                storage_type: "n".into(),
                value: Some("42".into()),
                formula: Some("SUM(B2,C2)".into())
            })
        );
        let shared = read_stored(xml.as_bytes(), "B2", 100).unwrap().unwrap();
        assert_eq!(shared.storage_type, "s");
        assert_eq!(shared.value.as_deref(), Some("3"));
        assert_eq!(read_stored(xml.as_bytes(), "C2", 100).unwrap(), None);
        assert_eq!(
            read_stored(sheet("<c r=\"A2\"/>").as_bytes(), "A2", 10)
                .unwrap()
                .unwrap()
                .value,
            None
        );
    }
    #[test]
    fn rejects_ambiguous_or_unbounded_cell_data() {
        for cells in [
            "<c r=\"A2\"/><c r=\"A2\"/>",
            "<c r=\"A2\"><v>1</v><v>2</v></c>",
            "<c r=\"A2\"><f t=\"shared\" si=\"0\"/></c>",
            "<c r=\"A2\" t=\"inlineStr\"><v>1</v></c>",
            "<c r=\"A2\"><v><x/></v></c>",
        ] {
            assert!(
                read_stored(sheet(cells).as_bytes(), "A2", 100).is_err(),
                "{cells}"
            );
        }
        assert!(read_stored(sheet("<c r=\"A2\"><v>123</v></c>").as_bytes(), "A2", 2).is_err());
        for address in [
            "a1",
            "$A$1",
            "A0",
            "A01",
            "XFE1",
            "A1048577",
            "A1:B2",
            "A١",
            "A99999999999999999999",
        ] {
            assert!(validate_address(address).is_err(), "{address}");
        }
        for address in ["A1", "Z99", "AA2", "XFD1048576"] {
            validate_address(address).unwrap();
        }
    }
}
