//! Insert a blank cell in ordered worksheet data before applying the typed edit.
//! Dimension and target-row spans are derived hints, omitted after insertion.
use crate::{ooxml_package::PackageResult, ooxml_relationships::attributes, xlsx_cells};
use quick_xml::{
    NsReader, Writer,
    events::{BytesStart, Event},
    name::ResolveResult,
};
const S: &[u8] = b"http://schemas.openxmlformats.org/spreadsheetml/2006/main";

fn position(address: &str) -> PackageResult<(u32, u32)> {
    xlsx_cells::validate_address(address)?;
    let split = address.bytes().take_while(u8::is_ascii_uppercase).count();
    let column = address[..split]
        .bytes()
        .fold(0, |n, c| n * 26 + u32::from(c - b'A' + 1));
    Ok((address[split..].parse()?, column))
}

fn blank(prefix: &str, address: &str) -> String {
    format!("<{prefix}c r=\"{address}\"/>")
}
fn row(prefix: &str, number: u32, address: &str) -> String {
    format!(
        "<{prefix}row r=\"{number}\">{}</{prefix}row>",
        blank(prefix, address)
    )
}

pub(crate) fn ensure(xml: &[u8], address: &str) -> PackageResult<Vec<u8>> {
    if xlsx_cells::read_stored(xml, address, 65_536)?.is_some() {
        return Ok(xml.to_vec());
    }
    let (target_row, target_column) = position(address)?;
    let mut reader = NsReader::from_reader(xml);
    let mut writer = Writer::new(Vec::new());
    let mut depth = 0usize;
    let mut data_prefix = None;
    let mut row_prefix = String::new();
    let mut current_row = 0;
    let mut last_row = 0;
    let mut last_column = 0;
    let mut inserted = false;
    let mut skip_dimension = None;
    loop {
        let (ns, event) = reader.read_resolved_event()?;
        let spreadsheet = matches!(ns, ResolveResult::Bound(ref n) if n.as_ref() == S);
        if let Some(skip_depth) = skip_dimension {
            match event {
                Event::Start(_) => depth += 1,
                Event::End(_) => {
                    depth -= 1;
                    if depth == skip_depth {
                        skip_dimension = None;
                    }
                }
                Event::Eof => return Err("incomplete worksheet dimension".into()),
                _ => {}
            }
            continue;
        }
        match event {
            Event::Start(ref element) | Event::Empty(ref element) => {
                let empty = matches!(event, Event::Empty(_));
                let local = element.local_name();
                let name = if spreadsheet { local.as_ref() } else { b"" };
                let qname = element.name();
                let qualified = std::str::from_utf8(qname.as_ref())?;
                if depth == 1 && name == b"dimension" {
                    if !empty {
                        skip_dimension = Some(depth);
                        depth += 1;
                    }
                    continue;
                }
                if depth == 1 && name == b"sheetData" {
                    let prefix = qualified
                        .strip_suffix("sheetData")
                        .ok_or("invalid sheetData name")?
                        .to_owned();
                    if empty {
                        writer.write_event(Event::Start(element.clone()))?;
                        writer
                            .get_mut()
                            .extend_from_slice(row(&prefix, target_row, address).as_bytes());
                        writer.write_event(Event::End(element.to_end()))?;
                        inserted = true;
                        continue;
                    }
                    data_prefix = Some(prefix);
                } else if data_prefix.is_some() && depth == 2 {
                    if name != b"row" {
                        return Err("unsupported worksheet data child".into());
                    }
                    let attrs = attributes(&reader, element)?;
                    let number = attrs
                        .get("r")
                        .ok_or("worksheet row requires explicit index")?;
                    if number.starts_with('0') || !number.bytes().all(|b| b.is_ascii_digit()) {
                        return Err("invalid worksheet row index".into());
                    }
                    current_row = number.parse()?;
                    if current_row <= last_row || current_row > 1_048_576 {
                        return Err("unordered or duplicate worksheet row".into());
                    }
                    last_row = current_row;
                    last_column = 0;
                    row_prefix = qualified
                        .strip_suffix("row")
                        .ok_or("invalid row name")?
                        .to_owned();
                    if !inserted && current_row > target_row {
                        writer.get_mut().extend_from_slice(
                            row(
                                data_prefix.as_deref().ok_or("missing data namespace")?,
                                target_row,
                                address,
                            )
                            .as_bytes(),
                        );
                        inserted = true;
                    }
                    if current_row == target_row {
                        let mut start = BytesStart::new(qualified);
                        for attr in element.attributes() {
                            let attr = attr?;
                            if attr.key.as_ref() != b"spans" {
                                start.push_attribute(attr);
                            }
                        }
                        writer.write_event(Event::Start(start))?;
                        if empty {
                            writer
                                .get_mut()
                                .extend_from_slice(blank(&row_prefix, address).as_bytes());
                            writer.write_event(Event::End(element.to_end()))?;
                            inserted = true;
                        } else {
                            depth += 1;
                        }
                        continue;
                    }
                } else if data_prefix.is_some() && depth == 3 {
                    if name != b"c" {
                        return Err("unsupported worksheet row child".into());
                    }
                    let attrs = attributes(&reader, element)?;
                    let (cell_row, column) =
                        position(attrs.get("r").ok_or("cell requires explicit address")?)?;
                    if cell_row != current_row || column <= last_column {
                        return Err("unordered or mismatched worksheet cell".into());
                    }
                    last_column = column;
                    if current_row == target_row && !inserted && column > target_column {
                        writer
                            .get_mut()
                            .extend_from_slice(blank(&row_prefix, address).as_bytes());
                        inserted = true;
                    }
                }
                if !empty {
                    depth += 1;
                }
                writer.write_event(event)?;
            }
            Event::End(_) => {
                if data_prefix.is_some() && depth == 3 && current_row == target_row && !inserted {
                    writer
                        .get_mut()
                        .extend_from_slice(blank(&row_prefix, address).as_bytes());
                    inserted = true;
                }
                if depth == 2
                    && let Some(prefix) = data_prefix.take()
                    && !inserted
                {
                    writer
                        .get_mut()
                        .extend_from_slice(row(&prefix, target_row, address).as_bytes());
                    inserted = true;
                }
                depth = depth.checked_sub(1).ok_or("invalid worksheet nesting")?;
                writer.write_event(event)?;
            }
            Event::Eof => break,
            _ => writer.write_event(event)?,
        }
    }
    if !inserted || depth != 0 {
        return Err("could not insert selected cell".into());
    }
    let output = writer.into_inner();
    if xlsx_cells::read_stored(&output, address, 65_536)?.is_none() {
        return Err("inserted cell readback failed".into());
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn worksheet(data: &str) -> String {
        format!(
            "<s:worksheet xmlns:s=\"{}\"><s:dimension ref=\"B2:D4\"/>{data}<s:pageMargins left=\"1\"/></s:worksheet>",
            std::str::from_utf8(S).unwrap()
        )
    }
    #[test]
    fn inserts_ordered_cells_and_rows_including_empty_data() {
        for (data, address, expected) in [
            (
                "<s:sheetData/>",
                "A1",
                "<s:row r=\"1\"><s:c r=\"A1\"/></s:row>",
            ),
            (
                "<s:sheetData><s:row r=\"2\" spans=\"2:4\"><s:c r=\"B2\"/><s:c r=\"D2\"/></s:row></s:sheetData>",
                "C2",
                "<s:c r=\"B2\"/><s:c r=\"C2\"/><s:c r=\"D2\"/>",
            ),
            (
                "<s:sheetData><s:row r=\"2\"/></s:sheetData>",
                "A2",
                "<s:row r=\"2\"><s:c r=\"A2\"/></s:row>",
            ),
            (
                "<s:sheetData><s:row r=\"2\"/></s:sheetData>",
                "A1",
                "<s:row r=\"1\"><s:c r=\"A1\"/></s:row><s:row r=\"2\"/>",
            ),
            (
                "<s:sheetData><s:row r=\"2\"/></s:sheetData>",
                "XFD1048576",
                "<s:row r=\"1048576\"><s:c r=\"XFD1048576\"/></s:row>",
            ),
        ] {
            let xml = worksheet(data);
            let output = String::from_utf8(ensure(xml.as_bytes(), address).unwrap()).unwrap();
            assert!(output.contains(expected), "{output}");
            assert!(!output.contains("dimension"));
            assert!(!output.contains("spans="));
            assert!(output.contains("<s:pageMargins left=\"1\"/>"));
            assert_eq!(
                ensure(output.as_bytes(), address).unwrap(),
                output.as_bytes()
            );
        }
    }
    #[test]
    fn rejects_ambiguous_rows_and_cell_coordinates() {
        for rows in [
            "<s:row r=\"2\"/><s:row r=\"2\"/>",
            "<s:row r=\"3\"/><s:row r=\"2\"/>",
            "<s:row/>",
            "<s:row r=\"2\"><s:c r=\"A3\"/></s:row>",
            "<s:row r=\"2\"><s:c r=\"D2\"/><s:c r=\"B2\"/></s:row>",
        ] {
            let xml = worksheet(&format!("<s:sheetData>{rows}</s:sheetData>"));
            assert!(ensure(xml.as_bytes(), "C2").is_err(), "{rows}");
        }
    }
}
