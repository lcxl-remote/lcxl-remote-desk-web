//! Preserve the prepared package and import only verified native formula caches.
use crate::{
    ooxml_package::{self, PackageResult},
    ooxml_relationships::attributes,
    xlsx_native_inputs, xlsx_native_preservation, xlsx_parts,
    xlsx_result::Scalar,
    xlsx_workbook_result::{self, CalculationReadback},
};
use quick_xml::{
    NsReader, Writer,
    events::{BytesStart, Event},
    name::ResolveResult,
};
use std::{
    collections::BTreeMap,
    io::{Cursor, Write},
};
const S: &[u8] = b"http://schemas.openxmlformats.org/spreadsheetml/2006/main";

pub fn merge(
    prepared: &[u8],
    native: &[u8],
    readback: &[CalculationReadback],
) -> PackageResult<Vec<u8>> {
    if readback.len() > 4096 {
        return Err("native result count exceeds bound".into());
    }
    xlsx_native_inputs::validate(prepared, native)?;
    xlsx_workbook_result::compare(prepared, native, readback)?;
    let mut parts = ooxml_package::read(prepared)?;
    let mut sheets: BTreeMap<String, BTreeMap<String, &Scalar>> = BTreeMap::new();
    for cell in readback {
        let selected = xlsx_parts::locate(&parts, &cell.sheet)?;
        if sheets
            .entry(selected.worksheet)
            .or_default()
            .insert(cell.address.clone(), &cell.value)
            .is_some()
        {
            return Err("duplicate native result".into());
        }
    }
    for (sheet, cells) in sheets {
        parts.insert(sheet.clone(), replace_caches(&parts[&sheet], &cells)?);
    }
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in &parts {
        zip.start_file(
            name,
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated),
        )?;
        zip.write_all(bytes)?;
    }
    let output = zip.finish()?.into_inner();
    if ooxml_package::read(&output)? != parts {
        return Err("merged package readback mismatch".into());
    }
    xlsx_workbook_result::compare(prepared, &output, readback)?;
    xlsx_native_preservation::validate(prepared, &output)?;
    Ok(output)
}

fn scalar(value: &Scalar) -> PackageResult<(&'static str, String)> {
    Ok(match value {
        Scalar::Number(number) if number.is_finite() => ("n", number.to_string()),
        Scalar::Number(_) => return Err("nonfinite native cache".into()),
        Scalar::Boolean(value) => ("b", if *value { "1" } else { "0" }.into()),
        Scalar::Text(text) => {
            if text.len() > 32768 {
                return Err("native text cache exceeds bound".into());
            }
            let escaped_prefix = text.replace("_x", "_x005F_x");
            let mut encoded = String::new();
            for ch in escaped_prefix.chars() {
                if ch == '\r' || ch.is_control() && !matches!(ch, '\n' | '\t') {
                    encoded.push_str(&format!("_x{:04X}_", ch as u32));
                } else {
                    encoded.push(ch);
                }
            }
            ("str", quick_xml::escape::escape(&encoded).into_owned())
        }
    })
}

struct Cell {
    start: usize,
    content: usize,
    depth: usize,
    opening: Vec<u8>,
    cache: String,
    old_value: Option<(usize, usize)>,
    value_start: Option<usize>,
}

fn replace_caches(xml: &[u8], targets: &BTreeMap<String, &Scalar>) -> PackageResult<Vec<u8>> {
    let mut reader = NsReader::from_reader(xml);
    let mut depth = 0usize;
    let mut active: Option<Cell> = None;
    let mut edits = Vec::new();
    let mut found = std::collections::HashSet::new();
    loop {
        let before = reader.buffer_position() as usize;
        let (ns, event) = reader.read_resolved_event()?;
        let is_sheet = matches!(ns, ResolveResult::Bound(ref ns) if ns.as_ref() == S);
        match event {
            Event::Start(ref element) | Event::Empty(ref element) => {
                let empty = matches!(event, Event::Empty(_));
                if is_sheet && element.local_name().as_ref() == b"c" {
                    let attrs = attributes(&reader, element)?;
                    if let Some((address, value)) = attrs
                        .get("r")
                        .and_then(|address| targets.get(address).map(|value| (address, value)))
                    {
                        if empty || active.is_some() || !found.insert(address.clone()) {
                            return Err("ambiguous formula cache target".into());
                        }
                        let qname = element.name();
                        let name = std::str::from_utf8(qname.as_ref())?;
                        let prefix = name.strip_suffix('c').ok_or("invalid formula cell name")?;
                        let (kind, content) = scalar(value)?;
                        let mut start = BytesStart::new(name);
                        for attribute in element.attributes() {
                            let attribute = attribute?;
                            if attribute.key.as_ref() != b"t" {
                                start.push_attribute(attribute);
                            }
                        }
                        start.push_attribute(("t", kind));
                        let mut writer = Writer::new(Vec::new());
                        writer.write_event(Event::Start(start))?;
                        active = Some(Cell {
                            start: before,
                            content: reader.buffer_position() as usize,
                            depth,
                            opening: writer.into_inner(),
                            cache: format!("<{prefix}v>{content}</{prefix}v>"),
                            old_value: None,
                            value_start: None,
                        });
                    }
                } else if is_sheet && element.local_name().as_ref() == b"v" {
                    if let Some(cell) = &mut active {
                        if depth != cell.depth + 1
                            || cell.old_value.is_some()
                            || cell.value_start.is_some()
                        {
                            return Err("ambiguous formula cache value".into());
                        }
                        if empty {
                            cell.old_value = Some((before, reader.buffer_position() as usize));
                        } else {
                            cell.value_start = Some(before);
                        }
                    }
                }
                if !empty {
                    depth += 1;
                }
            }
            Event::End(_) => {
                depth = depth.checked_sub(1).ok_or("invalid cache XML nesting")?;
                if let Some(cell) = &mut active {
                    if depth == cell.depth + 1 {
                        if let Some(start) = cell.value_start.take() {
                            cell.old_value = Some((start, reader.buffer_position() as usize));
                        }
                    }
                    if depth == cell.depth {
                        let mut cell = active.take().ok_or("missing cache cell")?;
                        if let Some((start, end)) = cell.old_value {
                            cell.opening.extend_from_slice(&xml[cell.content..start]);
                            cell.opening.extend_from_slice(&xml[end..before]);
                        } else {
                            cell.opening.extend_from_slice(&xml[cell.content..before]);
                        }
                        cell.opening.extend_from_slice(cell.cache.as_bytes());
                        cell.opening
                            .extend_from_slice(&xml[before..reader.buffer_position() as usize]);
                        edits.push((cell.start, reader.buffer_position() as usize, cell.opening));
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if depth != 0 || active.is_some() || found.len() != targets.len() {
        return Err("incomplete formula cache merge".into());
    }
    let mut result = Vec::new();
    let mut copied = 0;
    for (start, end, bytes) in edits {
        result.extend_from_slice(&xml[copied..start]);
        result.extend_from_slice(&bytes);
        copied = end;
        if result.len() > 4 * 1024 * 1024 {
            return Err("merged worksheet exceeds bound".into());
        }
    }
    result.extend_from_slice(&xml[copied..]);
    if result.len() > 4 * 1024 * 1024 {
        return Err("merged worksheet exceeds bound".into());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replaces_only_cache_and_storage_type_preserving_formula_style_and_other_cells() {
        let xml = br#"<s:worksheet xmlns:s="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><s:sheetData><s:row r="1"><s:c r="A1" s="4" t="n"><s:f>21*2</s:f><s:v>0</s:v></s:c><s:c r="B1"><s:v>21</s:v></s:c></s:row></s:sheetData></s:worksheet>"#;
        for value in [
            Scalar::Number(42.0),
            Scalar::Boolean(true),
            Scalar::Text("_x0041_ &\r\n测试".into()),
        ] {
            let result = replace_caches(xml, &BTreeMap::from([("A1".into(), &value)])).unwrap();
            let cells = crate::xlsx_cells::read_all(&result, 65536).unwrap();
            assert_eq!(cells["A1"].formula.as_deref(), Some("21*2"));
            assert_eq!(cells["B1"].value.as_deref(), Some("21"));
            assert_eq!(
                crate::xlsx_result::scalar(
                    &cells["A1"].storage_type,
                    cells["A1"].value.as_deref().unwrap()
                )
                .unwrap(),
                value
            );
            let text = std::str::from_utf8(&result).unwrap();
            assert!(text.contains("s=\"4\""));
            assert!(text.contains("<s:c r=\"B1\"><s:v>21</s:v></s:c>"));
        }
    }
    #[test]
    fn adds_missing_cache_and_refuses_missing_target_or_nonfinite_result() {
        let xml = br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><f>21*2</f></c></row></sheetData></worksheet>"#;
        let result =
            replace_caches(xml, &BTreeMap::from([("A1".into(), &Scalar::Number(42.0))])).unwrap();
        assert_eq!(
            crate::xlsx_cells::read_all(&result, 65536).unwrap()["A1"]
                .value
                .as_deref(),
            Some("42")
        );
        assert!(
            replace_caches(xml, &BTreeMap::from([("B1".into(), &Scalar::Number(42.0))])).is_err()
        );
        assert!(
            replace_caches(
                xml,
                &BTreeMap::from([("A1".into(), &Scalar::Number(f64::NAN))])
            )
            .is_err()
        );
    }
}
