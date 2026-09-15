//! Resolve a selected XLSX worksheet from relationships, not numbered filenames.
//! Call after the package boundary. This never authorizes opening native Excel.
use crate::{
    ooxml_package::PackageResult,
    ooxml_relationships::{self, Parts, attributes, relationships, target, unique, xml},
};
use quick_xml::{NsReader, events::Event, name::ResolveResult};
use std::collections::HashSet;
const S: &[u8] = b"http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const R: &[u8] = b"http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const SHEET: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet";
const STRINGS: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings";
const WORKBOOK_MIME: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml";
const SHEET_MIME: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml";
const STRINGS_MIME: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml";

#[derive(Debug, PartialEq, Eq)]
pub struct Selection {
    pub workbook: String,
    pub worksheet: String,
    pub shared_strings: Option<String>,
    pub sheet_name: String,
}

/// Workbook-order names, including hidden worksheets. Does not imply data disclosure consent.
pub(crate) fn sheet_names(parts: &Parts) -> PackageResult<Vec<String>> {
    let workbook = ooxml_relationships::main_part(parts)?;
    require_type(parts, &workbook, WORKBOOK_MIME)?;
    Ok(sheet_ids(xml(parts, &workbook)?)?
        .into_iter()
        .map(|(name, _)| name)
        .collect())
}

pub fn locate(parts: &Parts, sheet_name: &str) -> PackageResult<Selection> {
    let workbook = ooxml_relationships::main_part(parts)?;
    require_type(parts, &workbook, WORKBOOK_MIME)?;
    let sheets = sheet_ids(xml(parts, &workbook)?)?;
    let (_, id) = sheets
        .iter()
        .find(|(name, _)| name == sheet_name)
        .ok_or("selected worksheet does not exist")?;
    let links = relationships(parts, &workbook)?;
    let link = links
        .iter()
        .find(|link| &link.id == id && link.kind == SHEET)
        .ok_or("selected sheet is not a worksheet relationship")?;
    let worksheet = target(parts, &workbook, &link.target)?;
    require_type(parts, &worksheet, SHEET_MIME)?;
    let shared_strings = unique(&links, STRINGS)?
        .map(|link| target(parts, &workbook, &link.target))
        .transpose()?;
    if let Some(path) = &shared_strings {
        require_type(parts, path, STRINGS_MIME)?;
    }
    Ok(Selection {
        workbook,
        worksheet,
        shared_strings,
        sheet_name: sheet_name.into(),
    })
}

fn require_type(parts: &Parts, path: &str, expected: &str) -> PackageResult<()> {
    let mut reader = NsReader::from_str(xml(parts, "[Content_Types].xml")?);
    let mut found = false;
    loop {
        let (ns, event) = reader.read_resolved_event()?;
        match event {
            Event::Start(element) | Event::Empty(element)
                if matches!(ns, ResolveResult::Bound(ref ns) if ns.as_ref() == b"http://schemas.openxmlformats.org/package/2006/content-types")
                    && element.local_name().as_ref() == b"Override" =>
            {
                let attrs = attributes(&reader, &element)?;
                if attrs
                    .get("PartName")
                    .is_some_and(|name| name == &format!("/{path}"))
                {
                    if found || attrs.get("ContentType").map(String::as_str) != Some(expected) {
                        return Err("wrong or duplicate spreadsheet content type".into());
                    }
                    found = true;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !found {
        return Err("missing spreadsheet content type".into());
    }
    Ok(())
}

fn sheet_ids(text: &str) -> PackageResult<Vec<(String, String)>> {
    let mut reader = NsReader::from_str(text);
    reader.config_mut().expand_empty_elements = true;
    let mut depth = 0;
    let mut in_sheets = false;
    let mut seen_sheets = false;
    let mut names = HashSet::new();
    let mut ids = HashSet::new();
    let mut relationships = HashSet::new();
    let mut sheets = Vec::new();
    loop {
        let (ns, event) = reader.read_resolved_event()?;
        match event {
            Event::Start(element) => {
                depth += 1;
                let is_s = matches!(ns, ResolveResult::Bound(ref ns) if ns.as_ref() == S);
                let local = element.local_name();
                if depth == 1 && (!is_s || local.as_ref() != b"workbook") {
                    return Err("not a transitional spreadsheet workbook".into());
                }
                if is_s && local.as_ref() == b"workbookProtection" {
                    return Err("protected workbooks are unsupported".into());
                }
                if depth == 2 && is_s && local.as_ref() == b"sheets" {
                    if seen_sheets {
                        return Err("duplicate worksheet list".into());
                    }
                    in_sheets = true;
                    seen_sheets = true;
                } else if in_sheets && depth == 3 {
                    if !is_s || local.as_ref() != b"sheet" {
                        return Err("invalid worksheet list child".into());
                    }
                    let attrs = attributes(&reader, &element)?;
                    let name = attrs.get("name").ok_or("missing worksheet name")?;
                    if name.is_empty()
                        || name.chars().count() > 31
                        || name.chars().any(|c| {
                            c.is_control() || matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\')
                        })
                        || !names.insert(name.to_lowercase())
                    {
                        return Err("invalid or ambiguous worksheet name".into());
                    }
                    let id = attrs
                        .get("sheetId")
                        .and_then(|value| value.parse::<u32>().ok())
                        .filter(|id| *id > 0)
                        .ok_or("invalid worksheet id")?;
                    if !ids.insert(id) {
                        return Err("duplicate worksheet id".into());
                    }
                    let mut relationship = None;
                    for attr in element.attributes() {
                        let attr = attr?;
                        let (ns, local) = reader.resolve_attribute(attr.key);
                        if local.as_ref() == b"id"
                            && matches!(ns, ResolveResult::Bound(ref ns) if ns.as_ref() == R)
                        {
                            if relationship.is_some() {
                                return Err("ambiguous worksheet relationship".into());
                            }
                            relationship = Some(
                                attr.decode_and_unescape_value(reader.decoder())?
                                    .into_owned(),
                            );
                        }
                    }
                    let relationship = relationship
                        .filter(|id| !id.is_empty())
                        .ok_or("missing worksheet relationship")?;
                    if !relationships.insert(relationship.clone()) || sheets.len() >= 1024 {
                        return Err("ambiguous or oversized worksheet list".into());
                    }
                    sheets.push((name.clone(), relationship));
                } else if in_sheets && depth > 3 {
                    return Err("nested worksheet entry".into());
                }
            }
            Event::End(_) => {
                if depth == 2 {
                    in_sheets = false;
                }
                depth -= 1;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if sheets.is_empty() {
        return Err("workbook has no worksheets".into());
    }
    Ok(sheets)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn fixture() -> Parts {
        let mut parts = Parts::new();
        for (name, value) in [
            (
                "[Content_Types].xml",
                format!(
                    "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Override PartName=\"/custom/book.xml\" ContentType=\"{WORKBOOK_MIME}\"/><Override PartName=\"/data/chosen.xml\" ContentType=\"{SHEET_MIME}\"/></Types>"
                ),
            ),
            (
                "_rels/.rels",
                format!(
                    "<Relationships xmlns=\"{}\"><Relationship Id=\"root\" Type=\"{}\" Target=\"custom/book.xml\"/></Relationships>",
                    std::str::from_utf8(ooxml_relationships::PKG).unwrap(),
                    ooxml_relationships::OFFICE
                ),
            ),
            (
                "custom/book.xml",
                format!(
                    "<w:workbook xmlns:w=\"{}\" xmlns:link=\"{}\"><w:sheets><w:sheet name=\"数据 &amp; report\" sheetId=\"7\" link:id=\"chosen\"/></w:sheets></w:workbook>",
                    std::str::from_utf8(S).unwrap(),
                    std::str::from_utf8(R).unwrap()
                ),
            ),
            (
                "custom/_rels/book.xml.rels",
                format!(
                    "<Relationships xmlns=\"{}\"><Relationship Id=\"chosen\" Type=\"{SHEET}\" Target=\"../data/chosen.xml\"/></Relationships>",
                    std::str::from_utf8(ooxml_relationships::PKG).unwrap()
                ),
            ),
            (
                "data/chosen.xml",
                format!(
                    "<worksheet xmlns=\"{}\"><sheetData/></worksheet>",
                    std::str::from_utf8(S).unwrap()
                ),
            ),
        ] {
            parts.insert(name.into(), value.into_bytes());
        }
        parts
    }
    #[test]
    fn worksheet_identity_uses_name_namespace_and_relationship_not_numbering() {
        let parts = fixture();
        let before = parts.clone();
        assert_eq!(
            locate(&parts, "数据 & report").unwrap(),
            Selection {
                workbook: "custom/book.xml".into(),
                worksheet: "data/chosen.xml".into(),
                shared_strings: None,
                sheet_name: "数据 & report".into()
            }
        );
        assert!(locate(&parts, "Sheet1").is_err());
        assert_eq!(parts, before);
    }
    #[test]
    fn shared_strings_must_have_one_existing_typed_target() {
        let mut parts = fixture();
        let link = format!(
            "<Relationship Id=\"strings\" Type=\"{STRINGS}\" Target=\"../data/strings.xml\"/>"
        );
        let links = std::str::from_utf8(&parts["custom/_rels/book.xml.rels"])
            .unwrap()
            .replace("</Relationships>", &format!("{link}</Relationships>"));
        parts.insert(
            "custom/_rels/book.xml.rels".into(),
            links.clone().into_bytes(),
        );
        assert!(locate(&parts, "数据 & report").is_err());
        parts.insert(
            "data/strings.xml".into(),
            format!(
                "<sst xmlns=\"{}\"><si><t>共享文本</t></si></sst>",
                std::str::from_utf8(S).unwrap()
            )
            .into_bytes(),
        );
        assert!(locate(&parts, "数据 & report").is_err());
        let types = std::str::from_utf8(&parts["[Content_Types].xml"]).unwrap().replace("</Types>", &format!("<Override PartName=\"/data/strings.xml\" ContentType=\"{STRINGS_MIME}\"/></Types>"));
        parts.insert("[Content_Types].xml".into(), types.into_bytes());
        assert_eq!(
            locate(&parts, "数据 & report")
                .unwrap()
                .shared_strings
                .as_deref(),
            Some("data/strings.xml")
        );
        let duplicate = link.replace("Id=\"strings\"", "Id=\"strings2\"");
        parts.insert(
            "custom/_rels/book.xml.rels".into(),
            links
                .replace("</Relationships>", &format!("{duplicate}</Relationships>"))
                .into_bytes(),
        );
        assert!(locate(&parts, "数据 & report").is_err());
    }

    #[test]
    fn rejects_ambiguous_protected_or_wrong_type_workbooks() {
        for (path, from, to) in [
            ("custom/book.xml", "sheetId=\"7\"", "sheetId=\"0\""),
            ("custom/book.xml", "link:id=\"chosen\"", "id=\"chosen\""),
            (
                "custom/book.xml",
                "</w:sheets>",
                "<w:sheet name=\"数据 &amp; REPORT\" sheetId=\"8\" link:id=\"other\"/></w:sheets>",
            ),
            (
                "custom/book.xml",
                "</w:workbook>",
                "<w:workbookProtection/></w:workbook>",
            ),
            (
                "custom/_rels/book.xml.rels",
                "../data/chosen.xml",
                "../../escape.xml",
            ),
            (
                "custom/_rels/book.xml.rels",
                SHEET,
                "http://schemas.openxmlformats.org/officeDocument/2006/relationships/chartsheet",
            ),
            (
                "[Content_Types].xml",
                WORKBOOK_MIME,
                "application/vnd.ms-excel.sheet.macroEnabled.main+xml",
            ),
            ("[Content_Types].xml", SHEET_MIME, "application/xml"),
        ] {
            let mut parts = fixture();
            let changed = std::str::from_utf8(&parts[path]).unwrap().replace(from, to);
            parts.insert(path.into(), changed.into_bytes());
            assert!(locate(&parts, "数据 & report").is_err(), "{from} -> {to}");
        }
    }
}
