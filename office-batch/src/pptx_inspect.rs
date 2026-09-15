//! Read the same placeholder objects used by copy edits, without native Office.
use crate::{
    ooxml_package::{self, PackageResult},
    pptx_notes, pptx_parts,
    pptx_text::{self, Tag, tag},
};
use quick_xml::{NsReader, events::Event};

#[derive(Debug, PartialEq, Eq)]
pub struct Projection {
    pub slide_number: usize,
    pub title: Option<String>,
    pub presenter_notes: Option<String>,
    /// Format support only; never an authorization or runtime readiness signal.
    pub can_replace_title: bool,
    pub can_set_notes: bool,
}

pub fn inspect(
    bytes: &[u8],
    slide_number: usize,
    max_text_bytes: usize,
) -> PackageResult<Projection> {
    if max_text_bytes == 0 || max_text_bytes > 16384 {
        return Err("invalid presentation observation limit".into());
    }
    let parts = ooxml_package::read(bytes)?;
    let selected = pptx_parts::locate(&parts, slide_number)?;
    let title = placeholder(&parts[&selected.slide], true, max_text_bytes)?;
    let notes = selected
        .notes
        .as_ref()
        .map(|part| placeholder(&parts[part], false, max_text_bytes))
        .transpose()?
        .flatten();
    if title.as_ref().map_or(0, String::len) + notes.as_ref().map_or(0, String::len)
        > max_text_bytes
    {
        return Err("presentation text exceeds observation limit".into());
    }
    let can_replace_title = title
        .as_ref()
        .is_some_and(|text| pptx_text::replace(&parts[&selected.slide], true, text).is_ok());
    let can_set_notes = selected
        .notes
        .as_ref()
        .zip(notes.as_ref())
        .is_some_and(|(part, text)| pptx_notes::replace(&parts[part], text).is_ok());
    Ok(Projection {
        slide_number,
        title,
        presenter_notes: notes,
        can_replace_title,
        can_set_notes,
    })
}

fn placeholder(xml: &[u8], title: bool, limit: usize) -> PackageResult<Option<String>> {
    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().expand_empty_elements = true;
    let mut stack = Vec::new();
    let mut shape = None;
    let mut selected = false;
    let mut paragraphs = Vec::<String>::new();
    let mut text_bytes = 0usize;
    let mut unsupported = false;
    let mut candidates = Vec::new();
    loop {
        let (namespace, event) = reader.read_resolved_event()?;
        match event {
            Event::Start(element) => {
                let current = tag(namespace, element.local_name().as_ref());
                if current == Tag::Shape && stack.last() == Some(&Tag::Tree) {
                    if shape.is_some() {
                        return Err("nested presentation shape".into());
                    }
                    shape = Some(stack.len());
                    selected = false;
                    unsupported = false;
                    paragraphs.clear();
                    text_bytes = 0;
                }
                stack.push(current);
                if let Some(depth) = shape {
                    let path = &stack[depth..];
                    if path
                        == [
                            Tag::Shape,
                            Tag::NonVisual,
                            Tag::Properties,
                            Tag::Placeholder,
                        ]
                    {
                        for attr in element.attributes() {
                            let attr = attr?;
                            if attr.key.as_ref() == b"type" {
                                let value = attr.decode_and_unescape_value(reader.decoder())?;
                                selected |= if title {
                                    matches!(value.as_ref(), "title" | "ctrTitle")
                                } else {
                                    value == "body"
                                };
                            }
                        }
                    }
                    if path == [Tag::Shape, Tag::Body, Tag::Paragraph] {
                        text_bytes += usize::from(!paragraphs.is_empty());
                        paragraphs.push(String::new());
                    }
                    if path.len() == 4
                        && path[..3] == [Tag::Shape, Tag::Body, Tag::Paragraph]
                        && matches!(element.local_name().as_ref(), b"br" | b"fld")
                    {
                        unsupported = true;
                    }
                    if path.len() > 5 && path[4] == Tag::Text {
                        unsupported = true;
                    }
                }
            }
            Event::End(_) => {
                if shape == Some(stack.len().saturating_sub(1)) {
                    if selected {
                        if unsupported {
                            return Err("unsupported presentation text structure".into());
                        }
                        candidates.push(paragraphs.join("\n"));
                    }
                    shape = None;
                }
                stack.pop().ok_or("unmatched presentation end")?;
            }
            Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) => {
                if shape.is_some_and(|depth| {
                    stack[depth..] == [Tag::Shape, Tag::Body, Tag::Paragraph, Tag::Run, Tag::Text]
                }) {
                    let decoded = match event {
                        Event::Text(text) => text.xml_content()?.into_owned(),
                        Event::CData(text) => text.xml_content()?.into_owned(),
                        Event::GeneralRef(reference) => {
                            quick_xml::escape::unescape(&format!("&{};", reference.decode()?))?
                                .into_owned()
                        }
                        _ => unreachable!(),
                    };
                    let paragraph = paragraphs.last_mut().ok_or("text outside paragraph")?;
                    text_bytes += decoded.len();
                    paragraph.push_str(&decoded);
                }
            }
            Event::DocType(_) => return Err("presentation DTD is unsupported".into()),
            Event::Eof => break,
            _ => {}
        }
        if text_bytes > limit {
            return Err("presentation text exceeds observation limit".into());
        }
    }
    if !stack.is_empty() || candidates.len() > 1 {
        return Err("ambiguous presentation placeholder".into());
    }
    Ok(candidates.pop())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(kind: &str, text: &str) -> String {
        format!(
            "<p:notes xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\" xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:nvPr><p:ph type=\"{kind}\"/></p:nvPr></p:nvSpPr><p:txBody>{text}</p:txBody></p:sp></p:spTree></p:cSld></p:notes>"
        )
    }
    #[test]
    fn reads_runs_entities_and_empty_paragraphs_without_losing_text() {
        let xml = fixture(
            "body",
            "<a:p><a:r><a:t>中&amp;&#x1F600;</a:t></a:r><a:r><a:t><![CDATA[<>]]></a:t></a:r></a:p><a:p/><a:p><a:r><a:t>tail</a:t></a:r></a:p><a:p/>",
        );
        assert_eq!(
            placeholder(xml.as_bytes(), false, 100).unwrap().as_deref(),
            Some("中&😀<>\n\ntail\n")
        );
        assert!(placeholder(xml.as_bytes(), false, 4).is_err());
        assert_eq!(placeholder(xml.as_bytes(), true, 100).unwrap(), None);
        assert_eq!(
            placeholder(fixture("body", "<a:p/>").as_bytes(), false, 10)
                .unwrap()
                .as_deref(),
            Some("")
        );
    }
    #[test]
    fn refuses_duplicate_placeholders_and_dynamic_fields() {
        let xml = fixture("body", "<a:p/>");
        let shape = &xml[xml.find("<p:sp>").unwrap()..xml.find("</p:spTree>").unwrap()];
        assert!(
            placeholder(
                xml.replace("</p:spTree>", &format!("{shape}</p:spTree>"))
                    .as_bytes(),
                false,
                100
            )
            .is_err()
        );
        assert!(
            placeholder(
                fixture("body", "<a:p><a:fld><a:t>dynamic</a:t></a:fld></a:p>").as_bytes(),
                false,
                100
            )
            .is_err()
        );
    }
}
