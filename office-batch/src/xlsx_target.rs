//! Target-region checks before modifying existing or implicit worksheet cells.
//! These checks do not authorize native Excel activation or validate all package features.
use crate::{ooxml_package::PackageResult, ooxml_relationships::attributes, xlsx_cells};
use quick_xml::{NsReader, events::Event, name::ResolveResult};
use std::collections::BTreeMap;
const S: &[u8] = b"http://schemas.openxmlformats.org/spreadsheetml/2006/main";
type Position = (u32, u32);

fn position(address: &str) -> PackageResult<Position> {
    xlsx_cells::validate_address(address)?;
    let split = address.bytes().take_while(u8::is_ascii_uppercase).count();
    Ok((
        address[..split]
            .bytes()
            .fold(0, |n, c| n * 26 + u32::from(c - b'A' + 1)),
        address[split..].parse()?,
    ))
}
#[derive(Clone, Copy)]
struct Region {
    first: Position,
    last: Position,
}
impl Region {
    fn parse(text: &str) -> PackageResult<Self> {
        let (first, last) = text.split_once(':').unwrap_or((text, text));
        let first = position(first)?;
        let last = position(last)?;
        if first.0 > last.0 || first.1 > last.1 {
            return Err("inverted worksheet region".into());
        }
        Ok(Self { first, last })
    }
    fn contains(self, p: Position) -> bool {
        p.0 >= self.first.0 && p.0 <= self.last.0 && p.1 >= self.first.1 && p.1 <= self.last.1
    }
}

pub(crate) fn validate(xml: &[u8], address: &str) -> PackageResult<()> {
    let target = position(address)?;
    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().expand_empty_elements = true;
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut cell = None;
    let mut masters = BTreeMap::new();
    let mut shared = Vec::new();
    let mut merged = false;
    loop {
        let (ns, event) = reader.read_resolved_event()?;
        let spreadsheet = matches!(ns, ResolveResult::Bound(ref n) if n.as_ref() == S);
        match event {
            Event::Start(element) => {
                let local = element.local_name();
                if local.as_ref() == b"AlternateContent"
                    && matches!(ns, ResolveResult::Bound(ref n) if n.as_ref() == b"http://schemas.openxmlformats.org/markup-compatibility/2006")
                {
                    return Err("alternate worksheet representations require native review".into());
                }
                let name = if spreadsheet {
                    local.as_ref().to_vec()
                } else {
                    Vec::new()
                };
                match name.as_slice() {
                    b"sheetProtection" => return Err("protected worksheet cannot be edited".into()),
                    b"c" => {
                        if stack.len() != 3 || stack[1] != b"sheetData" || stack[2] != b"row" {
                            return Err("misplaced worksheet cell".into());
                        }
                        let attrs = attributes(&reader, &element)?;
                        cell = Some(position(
                            attrs
                                .get("r")
                                .ok_or("worksheet cell requires explicit address")?,
                        )?);
                    }
                    b"mergeCell" => {
                        if stack.len() != 2 || stack[1] != b"mergeCells" {
                            return Err("misplaced merged cell region".into());
                        }
                        let attrs = attributes(&reader, &element)?;
                        let region =
                            Region::parse(attrs.get("ref").ok_or("merged cell requires region")?)?;
                        if region.contains(target) {
                            if merged || region.first != target {
                                return Err("selected cell is covered by a merged region".into());
                            }
                            merged = true;
                        }
                    }
                    b"f" => {
                        if stack.len() != 4 || stack[3] != b"c" {
                            return Err("misplaced worksheet formula".into());
                        }
                        let cell = cell.ok_or("formula requires cell address")?;
                        let attrs = attributes(&reader, &element)?;
                        match attrs.get("t").map(String::as_str).unwrap_or("normal") {
                            "normal" => {
                                if attrs.contains_key("ref") {
                                    return Err("normal formula has unsupported region".into());
                                }
                            }
                            "shared" => {
                                let id = attrs.get("si").ok_or("shared formula requires index")?;
                                if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
                                    return Err("invalid shared formula index".into());
                                }
                                let id: u32 = id.parse()?;
                                shared.push((id, cell));
                                if let Some(reference) = attrs.get("ref") {
                                    let region = Region::parse(reference)?;
                                    if region.first != cell || masters.insert(id, region).is_some()
                                    {
                                        return Err("ambiguous shared formula master".into());
                                    }
                                }
                            }
                            "array" | "dataTable" => {
                                let region = Region::parse(
                                    attrs.get("ref").ok_or("formula requires explicit region")?,
                                )?;
                                if !region.contains(cell) || region.contains(target) {
                                    return Err("selected cell overlaps a formula region".into());
                                }
                            }
                            _ => return Err("unsupported worksheet formula type".into()),
                        }
                    }
                    _ => {}
                }
                stack.push(name);
            }
            Event::End(_) => {
                if stack.last().is_some_and(|n| n == b"c") {
                    cell = None;
                }
                stack.pop().ok_or("invalid worksheet nesting")?;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    for (id, cell) in shared {
        let region = masters.get(&id).ok_or("shared formula has no master")?;
        if !region.contains(cell) || region.contains(target) {
            return Err("selected cell overlaps shared formula region".into());
        }
    }
    if !stack.is_empty() {
        return Err("incomplete worksheet".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sheet(cells: &str, tail: &str) -> String {
        format!(
            "<worksheet xmlns=\"{}\"><sheetData><row r=\"1\">{cells}</row></sheetData>{tail}</worksheet>",
            std::str::from_utf8(S).unwrap()
        )
    }
    #[test]
    fn merged_anchor_is_editable_but_implicit_covered_cell_is_not() {
        let xml = sheet(
            "<c r=\"A1\"/>",
            "<mergeCells><mergeCell ref=\"A1:C3\"/></mergeCells>",
        );
        validate(xml.as_bytes(), "A1").unwrap();
        validate(xml.as_bytes(), "D1").unwrap();
        for address in ["B1", "A2", "C3"] {
            assert!(validate(xml.as_bytes(), address).is_err());
        }
        for reference in ["C3:A1", "A1:XFE1", "A1:B2:C3", "$A$1:$B$2"] {
            let xml = sheet(
                "",
                &format!("<mergeCells><mergeCell ref=\"{reference}\"/></mergeCells>"),
            );
            assert!(validate(xml.as_bytes(), "A1").is_err());
        }
    }
    #[test]
    fn array_and_shared_regions_protect_cells_without_formula_nodes() {
        for formula in [
            "<f t=\"array\" ref=\"A1:C3\">1</f>",
            "<f t=\"shared\" si=\"2\" ref=\"A1:C3\">1</f>",
            "<f t=\"dataTable\" ref=\"A1:C3\"/>",
        ] {
            let xml = sheet(&format!("<c r=\"A1\">{formula}<v>1</v></c>"), "");
            assert!(validate(xml.as_bytes(), "B2").is_err());
            validate(xml.as_bytes(), "D4").unwrap();
        }
        let xml = sheet(
            "<c r=\"A1\"><f t=\"shared\" si=\"2\" ref=\"A1:C1\">1</f></c><c r=\"B1\"><f t=\"shared\" si=\"2\"/></c>",
            "",
        );
        validate(xml.as_bytes(), "D1").unwrap();
        assert!(validate(xml.as_bytes(), "C1").is_err());
        let orphan = sheet("<c r=\"A1\"><f t=\"shared\" si=\"9\"/></c>", "");
        assert!(validate(orphan.as_bytes(), "D1").is_err());
    }
}
