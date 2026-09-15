//! Prepare an XLSX cell edit; the returned package is not a verified artifact.
//! All dependent formula results still require recalculation and independent readback.
use crate::{
    ooxml_package::{self, PackageResult},
    xlsx_cells, xlsx_insert, xlsx_parts, xlsx_target,
};
use quick_xml::{
    NsReader, Writer,
    events::{BytesStart, Event},
    name::ResolveResult,
};
use std::io::{Cursor, Write};
const S: &[u8] = b"http://schemas.openxmlformats.org/spreadsheetml/2006/main";

pub enum Value<'a> {
    Text(&'a str),
    Number(&'a str),
    Boolean(bool),
}

/// Must not be published or reported as successfully calculated. Preparing this
/// package does not authorize opening it in Excel, whose preflight is separate.
pub struct PendingRecalculation {
    pub bytes: Vec<u8>,
}

pub fn prepare_value(
    bytes: &[u8],
    sheet: &str,
    address: &str,
    value: Value<'_>,
) -> PackageResult<PendingRecalculation> {
    let (kind, content, expected) = value_content(value)?;
    prepare(bytes, sheet, address, kind, &content, Some(&expected), None)
}

pub(crate) fn prepare_formula_text(
    bytes: &[u8],
    sheet: &str,
    address: &str,
    formula: &str,
) -> PackageResult<PendingRecalculation> {
    let content = quick_xml::escape::escape(formula);
    prepare(bytes, sheet, address, "n", &content, None, Some(formula))
}

fn prepare(
    bytes: &[u8],
    sheet: &str,
    address: &str,
    kind: &str,
    content: &str,
    expected_value: Option<&str>,
    expected_formula: Option<&str>,
) -> PackageResult<PendingRecalculation> {
    let mut parts = ooxml_package::read(bytes)?;
    let selection = xlsx_parts::locate(&parts, sheet)?;
    let original = &parts[&selection.worksheet];
    xlsx_target::validate(original, address)?;
    let populated = xlsx_insert::ensure(original, address)?;
    let edited = replace_content(
        &populated,
        address,
        kind,
        content,
        expected_formula.is_some(),
    )?;
    let observed =
        xlsx_cells::read_stored(&edited, address, 65_536)?.ok_or("edited cell disappeared")?;
    if observed.storage_type != kind
        || observed.value.as_deref() != expected_value
        || observed.formula.as_deref() != expected_formula
    {
        return Err("edited cell readback mismatch".into());
    }
    parts.insert(selection.worksheet, edited);
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, part) in &parts {
        writer.start_file(name, zip::write::SimpleFileOptions::default())?;
        writer.write_all(part)?;
    }
    let output = writer.finish()?.into_inner();
    if ooxml_package::read(&output)? != parts {
        return Err("spreadsheet package readback mismatch".into());
    }
    Ok(PendingRecalculation { bytes: output })
}

fn value_content(value: Value<'_>) -> PackageResult<(&'static str, String, String)> {
    Ok(match value {
        Value::Text(text) => {
            if text.len() > 32_768
                || text
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
            {
                return Err("unsupported spreadsheet text value".into());
            }
            // Protect literal OOXML escape prefixes before encoding CR, which XML normalizes.
            let encoded = text.replace("_x", "_x005F_x").replace('\r', "_x000D_");
            (
                "inlineStr",
                quick_xml::escape::escape(&encoded).into_owned(),
                text.to_owned(),
            )
        }
        Value::Number(number) => {
            if number.is_empty()
                || number.len() > 128
                || number.trim() != number
                || !number.parse::<f64>()?.is_finite()
            {
                return Err("unsupported spreadsheet number".into());
            }
            (
                "n",
                quick_xml::escape::escape(number).into_owned(),
                number.to_owned(),
            )
        }
        Value::Boolean(value) => {
            let text = if value { "1" } else { "0" }.to_owned();
            ("b", text.clone(), text)
        }
    })
}

fn replace_content(
    xml: &[u8],
    address: &str,
    kind: &str,
    content: &str,
    formula: bool,
) -> PackageResult<Vec<u8>> {
    let mut reader = NsReader::from_reader(xml);
    let mut depth = 0usize;
    let mut selected = None;
    let mut span = None;
    let mut replacement = None;
    loop {
        let before = reader.buffer_position() as usize;
        let (ns, event) = reader.read_resolved_event()?;
        let spreadsheet = matches!(ns, ResolveResult::Bound(ref n) if n.as_ref() == S);
        match event {
            Event::Start(ref element) | Event::Empty(ref element) => {
                let empty = matches!(event, Event::Empty(_));
                if spreadsheet && element.local_name().as_ref() == b"sheetProtection" {
                    return Err("protected worksheet cannot be edited".into());
                }
                if spreadsheet && element.local_name().as_ref() == b"c" {
                    let mut is_target = false;
                    for attr in element.attributes() {
                        let attr = attr?;
                        if attr.key.as_ref() == b"r"
                            && attr.decode_and_unescape_value(reader.decoder())? == address
                        {
                            is_target = true;
                        }
                    }
                    if is_target {
                        if replacement.is_some() {
                            return Err("duplicate edited cell".into());
                        }
                        let qname = element.name();
                        let name = std::str::from_utf8(qname.as_ref())?;
                        let prefix = name.strip_suffix('c').ok_or("invalid cell name")?;
                        let mut start = BytesStart::new(name);
                        for attr in element.attributes() {
                            let attr = attr?;
                            let key = attr.key.as_ref();
                            if key == b"t" {
                                continue;
                            }
                            if !matches!(key, b"r" | b"s" | b"xmlns") && !key.starts_with(b"xmlns:")
                            {
                                return Err("unsupported edited cell metadata".into());
                            }
                            start.push_attribute(attr);
                        }
                        start.push_attribute(("t", kind));
                        let mut writer = Writer::new(Vec::new());
                        writer.write_event(Event::Start(start))?;
                        let payload = if formula {
                            format!("<{prefix}f>{content}</{prefix}f>")
                        } else if kind == "inlineStr" {
                            format!(
                                "<{prefix}is><{prefix}t xml:space=\"preserve\">{content}</{prefix}t></{prefix}is>"
                            )
                        } else {
                            format!("<{prefix}v>{content}</{prefix}v>")
                        };
                        writer.get_mut().extend_from_slice(payload.as_bytes());
                        writer
                            .get_mut()
                            .extend_from_slice(format!("</{name}>").as_bytes());
                        replacement = Some(writer.into_inner());
                        if empty {
                            span = Some((before, reader.buffer_position() as usize));
                        } else {
                            selected = Some((before, depth));
                        }
                    }
                }
                if !empty {
                    depth += 1;
                }
            }
            Event::End(_) => {
                depth = depth.checked_sub(1).ok_or("invalid worksheet nesting")?;
                if let Some((start, cell_depth)) = selected
                    && depth == cell_depth
                {
                    span = Some((start, reader.buffer_position() as usize));
                    selected = None;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let (start, end) = span.ok_or("selected cell has no complete span")?;
    let mut output = xml[..start].to_vec();
    output.extend_from_slice(&replacement.ok_or("missing replacement")?);
    output.extend_from_slice(&xml[end..]);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source(cells: &str) -> Vec<u8> {
        source_with_tail(cells, "")
    }
    fn source_with_tail(cells: &str, tail: &str) -> Vec<u8> {
        let mut parts = crate::xlsx_parts::tests::fixture();
        parts.insert("data/chosen.xml".into(), format!("<s:worksheet xmlns:s=\"{}\"><s:sheetData><s:row r=\"2\">{cells}</s:row></s:sheetData>{tail}</s:worksheet>", std::str::from_utf8(S).unwrap()).into_bytes());
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, value) in parts {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(&value).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }
    #[test]
    fn prepares_copy_preserving_styles_other_cells_and_parts_without_claiming_calculation() {
        let source = source(
            "<s:c r=\"A2\" s=\"4\"><s:f>1+1</s:f><s:v>2</s:v></s:c><s:c r=\"B2\"><s:f>A2*2</s:f><s:v>4</s:v></s:c>",
        );
        let before = ooxml_package::read(&source).unwrap();
        let prepared = prepare_value(&source, "数据 & report", "A2", Value::Number("21")).unwrap();
        let after = ooxml_package::read(&prepared.bytes).unwrap();
        for (name, value) in &before {
            if name != "data/chosen.xml" {
                assert_eq!(&after[name], value);
            }
        }
        let output = std::str::from_utf8(&after["data/chosen.xml"]).unwrap();
        assert!(output.contains("r=\"A2\" s=\"4\" t=\"n\""));
        assert!(output.contains("<s:c r=\"B2\"><s:f>A2*2</s:f><s:v>4</s:v></s:c>"));
        let observed = xlsx_cells::inspect_stored(&prepared.bytes, "数据 & report", "A2", 100)
            .unwrap()
            .unwrap();
        assert_eq!(observed.value.as_deref(), Some("21"));
        assert_eq!(observed.formula, None);
        assert_eq!(ooxml_package::read(&source).unwrap(), before);
    }
    #[test]
    fn text_is_literal_and_empty_cells_are_replaced_without_touching_neighbors() {
        let source = source("<s:c r=\"A2\"/><s:c r=\"B2\"><s:v>7</s:v></s:c>");
        let text = "=SUM(A1) & <中> _x0041_\r\n\t ";
        let output = prepare_value(&source, "数据 & report", "A2", Value::Text(text)).unwrap();
        assert_eq!(
            xlsx_cells::inspect(&output.bytes, "数据 & report", "A2", 100)
                .unwrap()
                .unwrap()
                .text
                .as_deref(),
            Some(text)
        );
        let output = prepare_value(&source, "数据 & report", "A2", Value::Boolean(true)).unwrap();
        assert_eq!(
            xlsx_cells::inspect_stored(&output.bytes, "数据 & report", "A2", 100)
                .unwrap()
                .unwrap()
                .value
                .as_deref(),
            Some("1")
        );
        let inserted = prepare_value(&source, "数据 & report", "C2", Value::Number("1")).unwrap();
        assert_eq!(
            xlsx_cells::inspect_stored(&inserted.bytes, "数据 & report", "C2", 100)
                .unwrap()
                .unwrap()
                .value
                .as_deref(),
            Some("1")
        );
    }
    #[test]
    fn rejects_covered_target_before_insertion_but_allows_unrelated_edit_and_merged_anchor() {
        for formula in [
            "<s:f t=\"array\" ref=\"A2:B3\">1</s:f>",
            "<s:f t=\"shared\" si=\"0\" ref=\"A2:B3\">1</s:f>",
        ] {
            let bytes = source(&format!("<s:c r=\"A2\">{formula}<s:v>1</s:v></s:c>"));
            let before = bytes.clone();
            assert!(prepare_value(&bytes, "数据 & report", "B3", Value::Number("9")).is_err());
            let approved = desk_diagnose_core::spreadsheet_formula::validate_formula_patch(
                "=1+1",
                "B3",
                desk_diagnose_core::spreadsheet_formula::FORMULA_LOCALE_V1,
                &["数据 & report".into()],
            )
            .unwrap();
            assert!(
                crate::xlsx_formula::prepare(
                    &bytes,
                    "数据 & report",
                    "B3",
                    "=1+1",
                    &approved.ast_digest_sha256
                )
                .is_err()
            );
            prepare_value(&bytes, "数据 & report", "C3", Value::Number("9")).unwrap();
            assert_eq!(bytes, before);
        }
        let merged = source_with_tail(
            "<s:c r=\"A2\"/>",
            "<s:mergeCells><s:mergeCell ref=\"A2:B3\"/></s:mergeCells>",
        );
        assert!(prepare_value(&merged, "数据 & report", "B3", Value::Text("hidden")).is_err());
        let anchor = prepare_value(&merged, "数据 & report", "A2", Value::Text("visible")).unwrap();
        assert_eq!(
            xlsx_cells::inspect(&anchor.bytes, "数据 & report", "A2", 100)
                .unwrap()
                .unwrap()
                .text
                .as_deref(),
            Some("visible")
        );
        assert!(
            std::str::from_utf8(&ooxml_package::read(&anchor.bytes).unwrap()["data/chosen.xml"])
                .unwrap()
                .contains("<s:mergeCell ref=\"A2:B3\"/>")
        );
    }
    #[test]
    fn refuses_invalid_values_protection_and_unhandled_metadata() {
        let bytes = source("<s:c r=\"A2\" cm=\"1\"/>");
        assert!(prepare_value(&bytes, "数据 & report", "A2", Value::Number("1")).is_err());
        let xml = format!(
            "<worksheet xmlns=\"{}\"><sheetData><row><c r=\"A2\"/></row></sheetData><sheetProtection/></worksheet>",
            std::str::from_utf8(S).unwrap()
        );
        assert!(replace_content(xml.as_bytes(), "A2", "n", "1", false).is_err());
        for number in ["NaN", "inf", "1e999", " 1", "1 ", "", "1,2"] {
            assert!(value_content(Value::Number(number)).is_err());
        }
        assert!(value_content(Value::Text("\0")).is_err());
    }
}
