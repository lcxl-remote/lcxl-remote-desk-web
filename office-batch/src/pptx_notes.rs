//! Plain-text notes replacement. Preserve the shape, rebuild only its paragraphs.
use crate::{
    ooxml_package::PackageResult,
    pptx_text::{Tag, tag},
};
use quick_xml::{NsReader, events::Event};

pub fn replace(xml: &[u8], text: &str) -> PackageResult<Vec<u8>> {
    if text.len() > 16384 || text.chars().any(|c| c.is_control() && c != '\n') {
        return Err("unsupported notes text".into());
    }
    let source = std::str::from_utf8(xml)?;
    let mut reader = NsReader::from_str(source);
    let mut stack = Vec::new();
    let mut shape = None;
    let mut selected = false;
    let mut paragraphs = Vec::new();
    let mut paragraph_start = None;
    let mut candidates = Vec::new();
    loop {
        let before = reader.buffer_position() as usize;
        let (ns, event) = reader.read_resolved_event()?;
        match event {
            Event::Start(ref element) | Event::Empty(ref element) => {
                let current = tag(ns, element.local_name().as_ref());
                let empty = matches!(event, Event::Empty(_));
                if current == Tag::Shape && stack.last() == Some(&Tag::Tree) && !empty {
                    if shape.is_some() {
                        return Err("nested notes shape".into());
                    }
                    shape = Some(stack.len());
                    selected = false;
                    paragraphs.clear();
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
                                selected |=
                                    attr.decode_and_unescape_value(reader.decoder())? == "body";
                            }
                        }
                    }
                    if path == [Tag::Shape, Tag::Body, Tag::Paragraph] {
                        if empty {
                            paragraphs.push(before..reader.buffer_position() as usize);
                        } else {
                            paragraph_start = Some(before);
                        }
                    }
                }
                if empty {
                    stack.pop();
                }
            }
            Event::End(_) => {
                if let Some(depth) = shape {
                    if stack[depth..] == [Tag::Shape, Tag::Body, Tag::Paragraph] {
                        let start = paragraph_start.take().ok_or("missing paragraph start")?;
                        paragraphs.push(start..reader.buffer_position() as usize);
                    }
                    if depth + 1 == stack.len() {
                        if selected {
                            if paragraphs.is_empty() {
                                return Err("missing notes paragraphs".into());
                            }
                            for pair in paragraphs.windows(2) {
                                if !source[pair[0].end..pair[1].start].trim().is_empty() {
                                    return Err(
                                        "non-paragraph content in notes paragraph range".into()
                                    );
                                }
                            }
                            candidates.push(paragraphs[0].start..paragraphs.last().unwrap().end);
                        }
                        shape = None;
                    }
                }
                stack.pop().ok_or("unmatched notes XML end")?;
            }
            Event::DocType(_) => return Err("notes DTD is unsupported".into()),
            Event::Eof => break,
            _ => {}
        }
    }
    if candidates.len() != 1 || !stack.is_empty() {
        return Err("missing or ambiguous notes body".into());
    }
    let range = &candidates[0];
    let mut output = Vec::new();
    output.extend_from_slice(&xml[..range.start]);
    // A locally declared prefix is independent of the input's namespace aliases.
    for line in text.split('\n') {
        output.extend_from_slice(b"<n:p xmlns:n=\"http://schemas.openxmlformats.org/drawingml/2006/main\"><n:r><n:t xml:space=\"preserve\">");
        output.extend_from_slice(quick_xml::escape::escape(line).as_bytes());
        output.extend_from_slice(b"</n:t></n:r></n:p>");
    }
    output.extend_from_slice(&xml[range.end..]);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(paragraphs: &str) -> String {
        format!(
            "<p:notes xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\" xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:nvPr><p:ph type=\"body\"/></p:nvPr></p:nvSpPr><p:txBody><a:bodyPr/><a:lstStyle/>{paragraphs}</p:txBody></p:sp></p:spTree></p:cSld></p:notes>"
        )
    }
    #[test]
    fn replaces_all_old_paragraphs_preserving_surrounding_bytes() {
        let old = "<a:p><a:r><a:t>old</a:t></a:r></a:p>\n<a:p/>";
        let xml = fixture(old);
        for (text, count) in [("第一段\n\n末段 & < >\n", 4), ("one", 1), ("", 1)] {
            let output = String::from_utf8(replace(xml.as_bytes(), text).unwrap()).unwrap();
            assert_eq!(output.matches("<n:p ").count(), count);
            let start = xml.find(old).unwrap();
            assert!(output.starts_with(&xml[..start]));
            assert!(output.ends_with(&xml[start + old.len()..]));
            assert!(!output.contains(">old<"));
            if count == 4 {
                assert!(output.contains("末段 &amp; &lt; &gt;"));
            }
        }
    }
    #[test]
    fn rejects_ambiguous_missing_and_interleaved_bodies() {
        let xml = fixture("<a:p/>");
        let shape = &xml[xml.find("<p:sp>").unwrap()..xml.find("</p:spTree>").unwrap()];
        for candidate in [
            xml.replace("type=\"body\"", "type=\"title\""),
            xml.replace("<a:p/>", ""),
            xml.replace("</p:spTree>", &format!("{shape}</p:spTree>")),
            xml.replace("<a:p/>", "<a:p/><a:unknown/><a:p/>"),
        ] {
            assert!(replace(candidate.as_bytes(), "new").is_err());
        }
        assert!(replace(xml.as_bytes(), "bad\0text").is_err());
    }
}
