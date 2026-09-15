//! Replace the document body while retaining its enclosing namespaces and final section.
use crate::ooxml_package::PackageResult;
use quick_xml::{NsReader, events::Event, name::ResolveResult};
const W: &[u8] = b"http://schemas.openxmlformats.org/wordprocessingml/2006/main";

pub(crate) fn replace(xml: &[u8], text: &str) -> PackageResult<Vec<u8>> {
    if text.len() > desk_agent_protocol::computer_use::MAX_LIVE_DOCUMENT_TEXT_BYTES
        || text.chars().any(|c| {
            (c.is_control() && !matches!(c, '\n' | '\t' | '\r'))
                || matches!(c, '\u{fffe}' | '\u{ffff}')
        })
    {
        return Err("invalid document replacement text".into());
    }
    let source = std::str::from_utf8(xml)?;
    let mut reader = NsReader::from_str(source);
    let mut depth = 0usize;
    let mut body = None;
    let mut body_start = None;
    let mut section = None;
    let mut section_start = None;
    let mut empty_body = None;
    loop {
        let before = reader.buffer_position() as usize;
        let (ns, event) = reader.read_resolved_event()?;
        let word = matches!(ns, ResolveResult::Bound(ref ns) if ns.as_ref() == W);
        match event {
            Event::Start(ref element) | Event::Empty(ref element) => {
                let empty = matches!(event, Event::Empty(_));
                let name = element.local_name();
                if depth == 0 && (!word || name.as_ref() != b"document" || empty) {
                    return Err("not a transitional Word document".into());
                }
                if depth == 1 && word && name.as_ref() == b"body" {
                    if body_start.is_some() || body.is_some() || empty_body.is_some() {
                        return Err("duplicate document body".into());
                    }
                    if empty {
                        empty_body = Some((
                            before,
                            reader.buffer_position() as usize,
                            String::from_utf8(element.name().as_ref().to_vec())?,
                        ));
                    } else {
                        body_start = Some(reader.buffer_position() as usize);
                    }
                } else if depth == 2 && body_start.is_some() {
                    if section.is_some() {
                        return Err("section properties must be the last body element".into());
                    }
                    if word && name.as_ref() == b"sectPr" {
                        if empty {
                            section = Some(before..reader.buffer_position() as usize);
                        } else {
                            section_start = Some(before);
                        }
                    }
                }
                if !empty {
                    depth += 1;
                }
            }
            Event::End(_) => {
                if depth == 3
                    && let Some(start) = section_start.take()
                {
                    section = Some(start..reader.buffer_position() as usize);
                }
                if depth == 2
                    && let Some(start) = body_start.take()
                {
                    body = Some(start..before);
                }
                depth = depth.checked_sub(1).ok_or("invalid document nesting")?;
            }
            Event::DocType(_) => return Err("document types are unsupported".into()),
            Event::Eof => break,
            _ => {}
        }
    }
    let mut paragraphs = String::new();
    // Preserve CR, LF and tab semantics with paragraphs and Word tab elements.
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    for line in normalized.split('\n') {
        paragraphs.push_str(
            "<w:p xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:r>",
        );
        for (index, part) in line.split('\t').enumerate() {
            if index != 0 {
                paragraphs.push_str("<w:tab/>");
            }
            paragraphs.push_str("<w:t xml:space=\"preserve\">");
            paragraphs.push_str(&quick_xml::escape::escape(part));
            paragraphs.push_str("</w:t>");
        }
        paragraphs.push_str("</w:r></w:p>");
    }
    let (range, replacement) = if let Some(range) = body {
        if let Some(section) = section {
            paragraphs.push_str(&source[section]);
        }
        (range, paragraphs)
    } else if let Some((start, end, name)) = empty_body {
        let opening = source[start..end]
            .strip_suffix("/>")
            .ok_or("invalid empty body")?;
        (start..end, format!("{opening}>{paragraphs}</{name}>"))
    } else {
        return Err("missing document body".into());
    };
    let mut output = source.to_owned();
    output.replace_range(range, &replacement);
    Ok(output.into_bytes())
}
