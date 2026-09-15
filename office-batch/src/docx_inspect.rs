//! Bounded stored body text; never evaluates fields or opens a Word application.
use crate::{
    docx_body, docx_parts,
    ooxml_package::{self, PackageResult},
};
use quick_xml::{NsReader, events::Event, name::ResolveResult};
const W: &[u8] = b"http://schemas.openxmlformats.org/wordprocessingml/2006/main";

#[derive(Debug, PartialEq, Eq)]
pub struct Projection {
    pub body_text: String,
}

pub fn inspect(bytes: &[u8], max_text_bytes: usize) -> PackageResult<Projection> {
    let parts = ooxml_package::read(bytes)?;
    let main = docx_parts::locate(&parts)?;
    Ok(Projection {
        body_text: body_text(&parts[&main], max_text_bytes)?,
    })
}

pub(crate) fn body_text(xml: &[u8], limit: usize) -> PackageResult<String> {
    if limit == 0 || limit > desk_agent_protocol::computer_use::MAX_LIVE_DOCUMENT_TEXT_BYTES {
        return Err("invalid Word observation limit".into());
    }
    // Use the same body and final-section structural constraints as mutation.
    docx_body::replace(xml, "")?;
    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().expand_empty_elements = true;
    let mut stack = Vec::new();
    let mut in_body = false;
    let mut paragraph = None;
    let mut seen_paragraph = false;
    let mut output = String::new();
    loop {
        let (ns, event) = reader.read_resolved_event()?;
        let word = matches!(ns, ResolveResult::Bound(ref ns) if ns.as_ref() == W);
        match event {
            Event::Start(element) => {
                let name = if word {
                    element.local_name().as_ref().to_vec()
                } else {
                    Vec::new()
                };
                if stack.len() == 1 && name == b"body" {
                    in_body = true;
                }
                if in_body {
                    if element.local_name().as_ref() == b"AlternateContent"
                        && matches!(ns, ResolveResult::Bound(ref ns) if ns.as_ref() == b"http://schemas.openxmlformats.org/markup-compatibility/2006")
                    {
                        return Err("alternate Word body representations are unsupported".into());
                    }
                    if matches!(
                        name.as_slice(),
                        b"del" | b"ins" | b"moveFrom" | b"moveTo" | b"altChunk"
                    ) {
                        return Err(
                            "Word revision or imported body content requires review in Word".into(),
                        );
                    }
                    if name == b"p" {
                        if paragraph.is_some() {
                            return Err("nested Word paragraphs are unsupported".into());
                        }
                        if seen_paragraph {
                            output.push('\n');
                        }
                        seen_paragraph = true;
                        paragraph = Some(stack.len());
                    }
                    if stack.last().is_some_and(|tag: &Vec<u8>| tag == b"t") {
                        return Err("nested markup in Word text".into());
                    }
                    if paragraph.is_some() {
                        match name.as_slice() {
                            b"tab" => output.push('\t'),
                            b"br" | b"cr" => output.push('\n'),
                            b"noBreakHyphen" => output.push('\u{2011}'),
                            b"softHyphen" => output.push('\u{00ad}'),
                            _ => {}
                        }
                    }
                }
                stack.push(name);
            }
            Event::End(_) => {
                if paragraph == Some(stack.len().saturating_sub(1)) {
                    paragraph = None;
                }
                if stack.len() == 2 && in_body {
                    in_body = false;
                }
                stack.pop().ok_or("invalid Word nesting")?;
            }
            Event::Text(_) | Event::CData(_) | Event::GeneralRef(_)
                if in_body && stack.last().is_some_and(|tag| tag == b"t") =>
            {
                if paragraph.is_none() {
                    return Err("Word text outside a paragraph".into());
                }
                let decoded = match event {
                    Event::Text(text) => text.xml_content()?.into_owned(),
                    Event::CData(text) => text.xml_content()?.into_owned(),
                    Event::GeneralRef(reference) => {
                        quick_xml::escape::unescape(&format!("&{};", reference.decode()?))?
                            .into_owned()
                    }
                    _ => unreachable!(),
                };
                output.push_str(&decoded);
            }
            Event::Eof => break,
            _ => {}
        }
        if output.len() > limit {
            return Err("Word body exceeds observation limit".into());
        }
    }
    Ok(output)
}
