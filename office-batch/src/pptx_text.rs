//! Exact text-span edits in a single unambiguous placeholder; no XML reformatting.
use crate::ooxml_package::PackageResult;
use quick_xml::{NsReader, events::Event, name::ResolveResult};
const P: &[u8] = b"http://schemas.openxmlformats.org/presentationml/2006/main";
const A: &[u8] = b"http://schemas.openxmlformats.org/drawingml/2006/main";
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tag {
    Tree,
    Shape,
    NonVisual,
    Properties,
    Placeholder,
    Body,
    Paragraph,
    Run,
    Text,
    Other,
}
pub(crate) fn tag(namespace: ResolveResult<'_>, name: &[u8]) -> Tag {
    match namespace {
        ResolveResult::Bound(ns) if ns.as_ref() == P => match name {
            b"spTree" => Tag::Tree,
            b"sp" => Tag::Shape,
            b"nvSpPr" => Tag::NonVisual,
            b"nvPr" => Tag::Properties,
            b"ph" => Tag::Placeholder,
            b"txBody" => Tag::Body,
            _ => Tag::Other,
        },
        ResolveResult::Bound(ns) if ns.as_ref() == A => match name {
            b"p" => Tag::Paragraph,
            b"r" => Tag::Run,
            b"t" => Tag::Text,
            _ => Tag::Other,
        },
        _ => Tag::Other,
    }
}
pub fn replace(xml: &[u8], title: bool, replacement: &str) -> PackageResult<Vec<u8>> {
    if replacement.len() > 16384 || replacement.chars().any(char::is_control) {
        return Err("unsupported replacement text".into());
    }
    let source = std::str::from_utf8(xml)?;
    let mut reader = NsReader::from_str(source);
    // Keep Empty events: text offsets must describe original bytes, not synthetic tags.
    let mut stack = Vec::new();
    let mut shape_depth = None;
    let mut selected = false;
    let mut paragraphs = 0;
    let mut unsupported = false;
    let mut texts = Vec::new();
    let mut text_start = None;
    let mut candidates = Vec::new();
    loop {
        let before = reader.buffer_position() as usize;
        let (namespace, event) = reader.read_resolved_event()?;
        match event {
            Event::Start(ref element) | Event::Empty(ref element) => {
                let current = tag(namespace, element.local_name().as_ref());
                let empty = matches!(event, Event::Empty(_));
                if text_start.is_some() {
                    return Err("nested markup inside text".into());
                }
                if !empty && current == Tag::Shape && stack.last() == Some(&Tag::Tree) {
                    if shape_depth.is_some() {
                        return Err("nested placeholder shape".into());
                    }
                    shape_depth = Some(stack.len());
                    selected = false;
                    paragraphs = 0;
                    unsupported = false;
                    texts.clear();
                }
                stack.push(current);
                if let Some(depth) = shape_depth {
                    let path = &stack[depth..];
                    if path
                        == [
                            Tag::Shape,
                            Tag::NonVisual,
                            Tag::Properties,
                            Tag::Placeholder,
                        ]
                    {
                        for attribute in element.attributes() {
                            let attribute = attribute?;
                            if attribute.key.as_ref() == b"type" {
                                let value =
                                    attribute.decode_and_unescape_value(reader.decoder())?;
                                selected |= if title {
                                    matches!(value.as_ref(), "title" | "ctrTitle")
                                } else {
                                    value == "body"
                                };
                            }
                        }
                    }
                    if path == [Tag::Shape, Tag::Body, Tag::Paragraph] {
                        paragraphs += 1;
                    }
                    if path.len() == 4
                        && path[..3] == [Tag::Shape, Tag::Body, Tag::Paragraph]
                        && current == Tag::Other
                        && matches!(element.local_name().as_ref(), b"br" | b"fld")
                    {
                        unsupported = true;
                    }
                    if path == [Tag::Shape, Tag::Body, Tag::Paragraph, Tag::Run, Tag::Text] {
                        if empty && selected {
                            return Err("empty text element requires structural editing".into());
                        }
                        if !empty {
                            text_start = Some(reader.buffer_position() as usize);
                        }
                    }
                }
                if empty {
                    stack.pop();
                }
            }
            Event::End(_) => {
                if stack.last() == Some(&Tag::Text)
                    && let Some(start) = text_start.take()
                {
                    texts.push(start..before);
                }
                if shape_depth == Some(stack.len().saturating_sub(1)) {
                    if selected {
                        if paragraphs != 1 || texts.is_empty() || texts.len() > 128 || unsupported {
                            return Err(
                                "complex placeholder text requires a richer edit contract".into()
                            );
                        }
                        candidates.push(texts.clone());
                    }
                    shape_depth = None;
                }
                stack.pop().ok_or("unmatched XML end")?;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if candidates.len() != 1 || !stack.is_empty() {
        return Err("missing or ambiguous placeholder".into());
    }
    let mut output = Vec::with_capacity(xml.len() + replacement.len());
    let mut remaining = replacement.chars();
    let mut cursor = 0;
    for (index, range) in candidates[0].iter().enumerate() {
        // Preserve run properties and allocate by decoded Unicode scalar count;
        // the final run receives any extension. Never split a UTF-8 character.
        let old = &source[range.clone()];
        if old.contains('<') {
            return Err("non-text markup in run".into());
        }
        let count = quick_xml::escape::unescape(old)?.chars().count();
        let replacement: String = if index + 1 == candidates[0].len() {
            remaining.by_ref().collect()
        } else {
            remaining.by_ref().take(count).collect()
        };
        output.extend_from_slice(&xml[cursor..range.start]);
        output.extend_from_slice(quick_xml::escape::escape(&replacement).as_bytes());
        cursor = range.end;
    }
    output.extend_from_slice(&xml[cursor..]);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(kind: &str) -> String {
        format!(
            "<p:sld xmlns:p=\"{}\" xmlns:a=\"{}\"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:nvPr><p:ph type=\"{kind}\"/></p:nvPr></p:nvSpPr><p:txBody><a:bodyPr/><a:p><a:r><a:rPr b=\"1\"/><a:t>old</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>",
            std::str::from_utf8(P).unwrap(),
            std::str::from_utf8(A).unwrap()
        )
    }
    #[test]
    fn changes_only_the_selected_text_bytes_and_escapes_new_text() {
        for (kind, title) in [("title", true), ("ctrTitle", true), ("body", false)] {
            let xml = fixture(kind);
            let result = replace(xml.as_bytes(), title, "中文 & <new>").unwrap();
            assert_eq!(
                String::from_utf8(result).unwrap(),
                xml.replace(">old<", ">中文 &amp; &lt;new&gt;<")
            );
        }
    }
    #[test]
    fn keeps_run_styles_and_allocates_text_by_original_character_boundaries() {
        let xml = fixture("title").replace(
            "old</a:t></a:r>",
            "A&amp;</a:t></a:r><a:r><a:rPr i=\"1\"/><a:t>tail</a:t></a:r>",
        );
        let result = String::from_utf8(replace(xml.as_bytes(), true, "中😀文&").unwrap()).unwrap();
        assert_eq!(
            result,
            xml.replace(">A&amp;<", ">中😀<")
                .replace(">tail<", ">文&amp;<")
        );
        let shorter = String::from_utf8(replace(xml.as_bytes(), true, "短").unwrap()).unwrap();
        assert_eq!(
            shorter,
            xml.replace(">A&amp;<", ">短<").replace(">tail<", "><")
        );
    }
    #[test]
    fn rejects_ambiguous_wrong_or_complex_placeholders_without_mutating_input() {
        let xml = fixture("title");
        for candidate in [
            xml.replace("type=\"title\"", "type=\"body\""),
            xml.replace("</a:p>", "</a:p><a:p><a:r><a:t>second</a:t></a:r></a:p>"),
            xml.replace("</a:p>", "<a:br/></a:p>"),
            xml.replace(
                "</a:p>",
                "<a:fld id=\"field\"><a:t>dynamic</a:t></a:fld></a:p>",
            ),
            xml.replace(
                "</p:spTree>",
                &format!(
                    "{}</p:spTree>",
                    &xml[xml.find("<p:sp>").unwrap()..xml.find("</p:spTree>").unwrap()]
                ),
            ),
            xml.replace("<a:t>old</a:t>", "<a:t/>"),
        ] {
            let before = candidate.clone();
            assert!(replace(candidate.as_bytes(), true, "new").is_err());
            assert_eq!(candidate, before);
        }
        assert!(replace(xml.as_bytes(), true, "line\nbreak").is_err());
    }
}
