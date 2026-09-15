//! Bounded SpreadsheetML rich strings; phonetic guides are not cell text.
use crate::ooxml_package::PackageResult;
use quick_xml::{NsReader, events::Event, name::ResolveResult};
const S: &[u8] = b"http://schemas.openxmlformats.org/spreadsheetml/2006/main";

pub(crate) struct RichText {
    stack: Vec<Vec<u8>>,
    plain: bool,
    runs: bool,
    run_text: bool,
    raw: String,
    limit: usize,
}
impl RichText {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            stack: Vec::new(),
            plain: false,
            runs: false,
            run_text: false,
            raw: String::new(),
            limit,
        }
    }
    pub(crate) fn start(&mut self, name: &[u8]) -> PackageResult<()> {
        let parent = self.stack.last().map(Vec::as_slice);
        if self.stack.iter().any(|n| n == b"rPr") {
            if name.is_empty() {
                return Err("foreign rich string formatting".into());
            }
        } else {
            match (parent, name) {
                (None, b"t") if !self.plain && !self.runs => self.plain = true,
                (None, b"r") if !self.plain => {
                    self.runs = true;
                    self.run_text = false;
                }
                (None, b"rPh" | b"phoneticPr") => {}
                (Some(b"r"), b"rPr") if !self.run_text => {}
                (Some(b"r"), b"t") if !self.run_text => self.run_text = true,
                (Some(b"rPh"), b"t") => {}
                _ => return Err("unsupported or ambiguous rich string structure".into()),
            }
        }
        self.stack.push(name.to_vec());
        Ok(())
    }
    pub(crate) fn end(&mut self) -> PackageResult<()> {
        self.stack.pop().ok_or("invalid rich string nesting")?;
        Ok(())
    }
    pub(crate) fn text(&mut self, text: &str) -> PackageResult<()> {
        if self.stack.iter().any(|n| n == b"rPh" || n == b"rPr") {
            return Ok(());
        }
        if self.stack.last().is_some_and(|n| n == b"t") {
            self.raw.push_str(text);
            // UTF-16 OOXML escapes can occupy seven bytes per code unit.
            if self.raw.len() > self.limit.saturating_mul(7) {
                return Err("rich string exceeds limit".into());
            }
        } else if !text.trim().is_empty() {
            return Err("text outside rich string run".into());
        }
        Ok(())
    }
    pub(crate) fn finish(self) -> PackageResult<String> {
        if !self.stack.is_empty() {
            return Err("incomplete rich string".into());
        }
        decode(&self.raw, self.limit)
    }
}

/// Decode each OOXML escape once, so _x005F_x0041_ remains literal _x0041_.
pub(crate) fn decode(raw: &str, limit: usize) -> PackageResult<String> {
    let mut units = Vec::new();
    let mut rest = raw;
    while !rest.is_empty() {
        let bytes = rest.as_bytes();
        if bytes.len() >= 7
            && &bytes[..2] == b"_x"
            && bytes[6] == b'_'
            && bytes[2..6].iter().all(u8::is_ascii_hexdigit)
        {
            units.push(u16::from_str_radix(&rest[2..6], 16)?);
            rest = &rest[7..];
        } else {
            let ch = rest.chars().next().ok_or("invalid string character")?;
            let mut buffer = [0; 2];
            units.extend_from_slice(ch.encode_utf16(&mut buffer));
            rest = &rest[ch.len_utf8()..];
        }
    }
    let text = String::from_utf16(&units)?;
    if text.len() > limit {
        return Err("decoded string exceeds observation limit".into());
    }
    Ok(text)
}

pub(crate) fn shared(xml: &[u8], index: usize, limit: usize) -> PackageResult<String> {
    read_shared(xml, Some(index), limit)?
        .into_iter()
        .next()
        .ok_or_else(|| "shared string index does not exist".into())
}

pub(crate) fn shared_all(xml: &[u8], limit: usize) -> PackageResult<Vec<String>> {
    read_shared(xml, None, limit)
}

fn read_shared(xml: &[u8], requested: Option<usize>, limit: usize) -> PackageResult<Vec<String>> {
    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().expand_empty_elements = true;
    let mut depth: usize = 0;
    let mut root = false;
    let mut count = 0;
    let mut selected: Option<RichText> = None;
    let mut result = Vec::new();
    loop {
        let (ns, event) = reader.read_resolved_event()?;
        let valid_ns = matches!(ns, ResolveResult::Bound(ref n) if n.as_ref() == S);
        match event {
            Event::Start(element) => {
                let local = element.local_name();
                let name = if valid_ns { local.as_ref() } else { b"" };
                if depth == 0 {
                    if root || name != b"sst" {
                        return Err("invalid shared string root".into());
                    }
                    root = true;
                } else if depth == 1 {
                    if name != b"si" {
                        return Err("unsupported shared string entry".into());
                    }
                    if requested.is_none_or(|index| count == index) {
                        selected = Some(RichText::new(limit));
                    }
                    count += 1;
                } else if let Some(value) = &mut selected {
                    value.start(name)?;
                }
                depth += 1;
            }
            Event::End(_) => {
                if depth == 2 {
                    if let Some(value) = selected.take() {
                        result.push(value.finish()?);
                    }
                } else if depth > 2
                    && let Some(value) = &mut selected
                {
                    value.end()?;
                }
                depth = depth
                    .checked_sub(1)
                    .ok_or("invalid shared string nesting")?;
            }
            Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) => {
                if let Some(value) = &mut selected {
                    value.text(&event_text(event)?)?;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !root || depth != 0 {
        return Err("incomplete shared strings".into());
    }
    Ok(result)
}

pub(crate) fn event_text(event: Event<'_>) -> PackageResult<String> {
    Ok(match event {
        Event::Text(t) => t.xml_content()?.into_owned(),
        Event::CData(t) => t.xml_content()?.into_owned(),
        Event::GeneralRef(r) => {
            quick_xml::escape::unescape(&format!("&{};", r.decode()?))?.into_owned()
        }
        _ => return Err("expected string text event".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resolves_selected_rich_string_without_phonetic_or_formatting_text() {
        let xml = format!(
            "<s:sst xmlns:s=\"{}\"><s:si><s:t>other</s:t></s:si><s:si><s:r><s:rPr><s:b/></s:rPr><s:t>中&amp;</s:t></s:r><s:r><s:t>_xD83D__xDE00_</s:t></s:r><s:rPh sb=\"0\" eb=\"1\"><s:t>phonetic</s:t></s:rPh></s:si></s:sst>",
            std::str::from_utf8(S).unwrap()
        );
        assert_eq!(shared(xml.as_bytes(), 1, 8).unwrap(), "中&😀");
        assert!(shared(xml.as_bytes(), 1, 7).is_err());
        assert!(shared(xml.as_bytes(), 2, 100).is_err());
        assert_eq!(decode("_x005F_x0041_", 20).unwrap(), "_x0041_");
        assert!(decode("_xD800_", 20).is_err());
    }
    #[test]
    fn refuses_ambiguous_and_foreign_selected_text() {
        for body in [
            "<t>a</t><t>b</t>",
            "<t>a</t><r><t>b</t></r>",
            "<r><t>a</t><t>b</t></r>",
            "<t xmlns=\"urn:foreign\">x</t>",
            "<t><r/></t>",
            "unwrapped",
        ] {
            let xml = format!(
                "<sst xmlns=\"{}\"><si>{body}</si></sst>",
                std::str::from_utf8(S).unwrap()
            );
            assert!(shared(xml.as_bytes(), 0, 100).is_err(), "{body}");
        }
    }
}
