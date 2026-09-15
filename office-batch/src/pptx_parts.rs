//! Resolve transitional PPTX parts from package relationships, never file numbering.
use crate::ooxml_package::PackageResult;
#[cfg(test)]
use crate::ooxml_relationships::PKG;
use crate::ooxml_relationships::{
    OFFICE, relationships, relationships_optional, target, unique, xml,
};
use quick_xml::{NsReader, events::Event, name::ResolveResult};
use std::collections::{BTreeMap, HashSet};
const P: &[u8] = b"http://schemas.openxmlformats.org/presentationml/2006/main";
const R: &[u8] = b"http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const SLIDE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide";
const NOTES: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide";
type Parts = BTreeMap<String, Vec<u8>>;
#[derive(Debug, PartialEq, Eq)]
pub struct Selection {
    pub slide: String,
    pub notes: Option<String>,
}

pub fn locate(parts: &Parts, slide_number: usize) -> PackageResult<Selection> {
    let roots = relationships(parts, "")?;
    let root = unique(&roots, OFFICE)?.ok_or("no presentation relationship")?;
    let presentation = target(parts, "", &root.target)?;
    let ids = slide_ids(xml(parts, &presentation)?)?;
    let id = slide_number
        .checked_sub(1)
        .and_then(|index| ids.get(index))
        .ok_or("slide number is out of range")?;
    let links = relationships(parts, &presentation)?;
    let link = links
        .iter()
        .find(|link| &link.id == id && link.kind == SLIDE)
        .ok_or("missing or wrong slide relationship")?;
    let slide = target(parts, &presentation, &link.target)?;
    let links = relationships_optional(parts, &slide)?;
    let notes = unique(&links, NOTES)?
        .map(|link| target(parts, &slide, &link.target))
        .transpose()?;
    Ok(Selection { slide, notes })
}
fn slide_ids(xml: &str) -> PackageResult<Vec<String>> {
    let mut reader = NsReader::from_str(xml);
    reader.config_mut().expand_empty_elements = true;
    let mut depth = 0;
    let mut list = false;
    let mut seen_list = false;
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    loop {
        let (namespace, event) = reader.read_resolved_event()?;
        match event {
            Event::Start(element) => {
                depth += 1;
                let presentation_ns =
                    matches!(namespace, ResolveResult::Bound(ref ns) if ns.as_ref() == P);
                if depth == 1
                    && (!presentation_ns || element.local_name().as_ref() != b"presentation")
                {
                    return Err("not a transitional presentation".into());
                }
                if depth == 2 && presentation_ns && element.local_name().as_ref() == b"sldIdLst" {
                    if seen_list {
                        return Err("duplicate slide list".into());
                    }
                    seen_list = true;
                    list = true;
                }
                if depth == 3 && list {
                    if !presentation_ns || element.local_name().as_ref() != b"sldId" {
                        return Err("unexpected slide list child".into());
                    }
                    let mut id = None;
                    for attribute in element.attributes() {
                        let attribute = attribute?;
                        let (ns, local) = reader.resolve_attribute(attribute.key);
                        if local.as_ref() == b"id"
                            && matches!(ns, ResolveResult::Bound(ref ns) if ns.as_ref() == R)
                        {
                            id = Some(
                                attribute
                                    .decode_and_unescape_value(reader.decoder())?
                                    .into_owned(),
                            );
                        }
                    }
                    let id = id
                        .filter(|value| !value.is_empty())
                        .ok_or("missing slide relationship ID")?;
                    if !seen.insert(id.clone()) {
                        return Err("duplicate slide relationship ID".into());
                    }
                    ids.push(id);
                }
            }
            Event::End(_) => {
                if depth == 2 {
                    list = false;
                }
                depth -= 1;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(ids)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn rel(id: &str, kind: &str, target: &str) -> String {
        format!("<Relationship Id=\"{id}\" Type=\"{kind}\" Target=\"{target}\"/>")
    }
    fn rels(children: &str) -> Vec<u8> {
        format!(
            "<Relationships xmlns=\"{}\">{children}</Relationships>",
            std::str::from_utf8(PKG).unwrap()
        )
        .into_bytes()
    }
    fn fixture() -> Parts {
        BTreeMap::from([
            ("_rels/.rels".into(), rels(&rel("root", OFFICE, "deck/main.xml"))),
            ("deck/main.xml".into(), format!("<p:presentation xmlns:p=\"{}\" xmlns:link=\"{}\"><p:sldIdLst><p:sldId id=\"257\" link:id=\"second\"/><p:sldId id=\"256\" link:id=\"first\"/></p:sldIdLst></p:presentation>", std::str::from_utf8(P).unwrap(), std::str::from_utf8(R).unwrap()).into_bytes()),
            ("deck/_rels/main.xml.rels".into(), rels(&(rel("first", SLIDE, "slides/a.xml") + &rel("second", SLIDE, "slides/z.xml")))),
            ("deck/slides/a.xml".into(), b"<slide/>".to_vec()),
            ("deck/slides/z.xml".into(), b"<slide/>".to_vec()),
            ("deck/slides/_rels/z.xml.rels".into(), rels(&rel("notes", NOTES, "../notes/custom.xml"))),
            ("deck/notes/custom.xml".into(), b"<notes/>".to_vec()),
        ])
    }
    #[test]
    fn follows_presentation_order_custom_part_names_and_namespace_aliases() {
        let parts = fixture();
        let before = parts.clone();
        assert_eq!(
            locate(&parts, 1).unwrap(),
            Selection {
                slide: "deck/slides/z.xml".into(),
                notes: Some("deck/notes/custom.xml".into())
            }
        );
        assert_eq!(
            locate(&parts, 2).unwrap(),
            Selection {
                slide: "deck/slides/a.xml".into(),
                notes: None
            }
        );
        assert_eq!(parts, before);
        assert!(locate(&parts, 0).is_err());
        assert!(locate(&parts, 3).is_err());
    }
    #[test]
    fn rejects_wrong_missing_duplicate_and_external_relationships() {
        for link in [
            rel("second", NOTES, "slides/z.xml"),
            rel("second", SLIDE, "missing.xml"),
            rel("second", SLIDE, "../../escape.xml"),
            rel("second", SLIDE, "https://example.invalid/a"),
            rel("second", SLIDE, "slides/z.xml") + &rel("second", SLIDE, "slides/a.xml"),
        ] {
            let mut parts = fixture();
            parts.insert("deck/_rels/main.xml.rels".into(), rels(&link));
            assert!(locate(&parts, 1).is_err());
        }
        let mut parts = fixture();
        parts.insert(
            "deck/slides/_rels/z.xml.rels".into(),
            rels(
                &(rel("one", NOTES, "../notes/custom.xml")
                    + &rel("two", NOTES, "../notes/custom.xml")),
            ),
        );
        assert!(locate(&parts, 1).is_err());
    }
    #[test]
    fn namespace_spoof_and_duplicate_slide_references_are_not_selectors() {
        for replacement in ["wrong", "first"] {
            let mut parts = fixture();
            let xml = String::from_utf8(parts["deck/main.xml"].clone()).unwrap();
            let xml = if replacement == "wrong" {
                xml.replace(std::str::from_utf8(R).unwrap(), "urn:wrong")
            } else {
                xml.replace("link:id=\"second\"", "link:id=\"first\"")
            };
            parts.insert("deck/main.xml".into(), xml.into_bytes());
            assert!(locate(&parts, 1).is_err());
        }
    }
}
