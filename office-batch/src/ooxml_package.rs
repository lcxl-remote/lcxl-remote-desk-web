//! In-memory package boundary for direct OOXML experiments. Never extracts paths.
//! This is not sufficient authorization to open an arbitrary package in Office.
use std::{
    collections::{BTreeMap, HashSet},
    io::{Cursor, Read},
};
pub type PackageResult<T> = Result<T, Box<dyn std::error::Error>>;
const MAX_ARCHIVE: usize = 16 * 1024 * 1024;
const MAX_PART: usize = 4 * 1024 * 1024;
const MAX_EXPANDED: usize = 32 * 1024 * 1024;

pub fn read(bytes: &[u8]) -> PackageResult<BTreeMap<String, Vec<u8>>> {
    if bytes.len() > MAX_ARCHIVE {
        return Err("OOXML archive exceeds limit".into());
    }
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    if archive.len() > 1024 {
        return Err("too many OOXML parts".into());
    }
    let mut names = HashSet::new();
    let mut parts = BTreeMap::new();
    let mut total = 0usize;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let name = std::str::from_utf8(entry.name_raw())?.to_owned();
        let folded = name.to_ascii_lowercase();
        if !valid_name(&name) || !names.insert(folded.clone()) || entry.encrypted() {
            return Err("ambiguous or unsafe OOXML part".into());
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| !matches!(mode & 0o170000, 0 | 0o100000 | 0o040000))
        {
            return Err("nonregular OOXML part".into());
        }
        if entry.is_dir() {
            continue;
        }
        if ["/embeddings/", "/activex/", "/externallinks/"]
            .iter()
            .any(|value| folded.contains(value))
        {
            return Err("active or embedded OOXML content is unsupported".into());
        }
        if entry.size() > MAX_PART as u64 {
            return Err("OOXML part exceeds limit".into());
        }
        let mut data = Vec::new();
        entry
            .by_ref()
            .take(MAX_PART as u64 + 1)
            .read_to_end(&mut data)?;
        total = total
            .checked_add(data.len())
            .ok_or("expanded package overflow")?;
        if data.len() > MAX_PART || total > MAX_EXPANDED {
            return Err("expanded OOXML exceeds limit".into());
        }
        if folded.ends_with(".xml") || folded.ends_with(".rels") {
            inspect_xml(&data)?;
        }
        parts.insert(name, data);
    }
    if !parts.contains_key("[Content_Types].xml") || !parts.contains_key("_rels/.rels") {
        return Err("missing OOXML package metadata".into());
    }
    for name in parts
        .keys()
        .filter(|name| name.to_ascii_lowercase().ends_with(".bin"))
    {
        if !printer_settings(name, &parts["[Content_Types].xml"])? {
            return Err("unsupported binary OOXML part".into());
        }
    }
    Ok(parts)
}

fn printer_settings(name: &str, content_types: &[u8]) -> PackageResult<bool> {
    const MIME: &str =
        "application/vnd.openxmlformats-officedocument.presentationml.printerSettings";
    let Some(number) = name
        .strip_prefix("ppt/printerSettings/printerSettings")
        .and_then(|value| value.strip_suffix(".bin"))
    else {
        return Ok(false);
    };
    if number.is_empty() || !number.bytes().all(|value| value.is_ascii_digit()) {
        return Ok(false);
    }
    let mut reader = quick_xml::NsReader::from_str(std::str::from_utf8(content_types)?);
    reader.config_mut().expand_empty_elements = true;
    let mut defaults = BTreeMap::new();
    let mut overrides = BTreeMap::new();
    let mut depth = 0;
    loop {
        use quick_xml::{events::Event, name::ResolveResult};
        let (namespace, event) = reader.read_resolved_event()?;
        match event {
            Event::Start(element) => {
                depth += 1;
                if !matches!(namespace, ResolveResult::Bound(ref ns) if ns.as_ref() == b"http://schemas.openxmlformats.org/package/2006/content-types")
                {
                    return Err("wrong content-types namespace".into());
                }
                if depth == 1 {
                    if element.local_name().as_ref() != b"Types" {
                        return Err("wrong content-types root".into());
                    }
                } else {
                    if depth != 2 {
                        return Err("nested content type".into());
                    }
                    let mut attrs = BTreeMap::new();
                    for attribute in element.attributes() {
                        let attribute = attribute?;
                        attrs.insert(
                            std::str::from_utf8(attribute.key.as_ref())?.to_owned(),
                            attribute
                                .decode_and_unescape_value(reader.decoder())?
                                .into_owned(),
                        );
                    }
                    let mime = attrs
                        .get("ContentType")
                        .ok_or("missing content type")?
                        .clone();
                    match element.local_name().as_ref() {
                        b"Default" => {
                            let extension = attrs
                                .get("Extension")
                                .ok_or("missing extension")?
                                .to_ascii_lowercase();
                            if defaults.insert(extension, mime).is_some() {
                                return Err("duplicate default content type".into());
                            }
                        }
                        b"Override" => {
                            let path = attrs
                                .get("PartName")
                                .ok_or("missing part name")?
                                .to_ascii_lowercase();
                            if overrides.insert(path, mime).is_some() {
                                return Err("duplicate override content type".into());
                            }
                        }
                        _ => return Err("unknown content type entry".into()),
                    }
                }
            }
            Event::End(_) => depth -= 1,
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(overrides
        .get(&format!("/{name}").to_ascii_lowercase())
        .or_else(|| defaults.get("bin"))
        .is_some_and(|value| value == MIME))
}

fn valid_name(name: &str) -> bool {
    let trimmed = name.strip_suffix('/').unwrap_or(name);
    !trimmed.is_empty()
        && name.len() <= 512
        && !name
            .chars()
            .any(|value| value.is_control() || matches!(value, '\\' | ':' | '%' | '?' | '#'))
        && trimmed
            .split('/')
            .all(|part| !matches!(part, "" | "." | ".."))
}

fn inspect_xml(bytes: &[u8]) -> PackageResult<()> {
    let text = std::str::from_utf8(bytes)?;
    let mut reader = quick_xml::Reader::from_str(text);
    reader.config_mut().expand_empty_elements = true;
    let mut depth = 0usize;
    let mut roots = 0usize;
    loop {
        use quick_xml::events::Event;
        match reader.read_event()? {
            Event::DocType(_) => return Err("DTD is unsupported in OOXML".into()),
            Event::Start(element) | Event::Empty(element) => {
                if depth == 0 {
                    roots += 1;
                    if roots > 1 {
                        return Err("multiple XML roots".into());
                    }
                }
                depth += 1;
                if depth > 128 {
                    return Err("XML nesting exceeds limit".into());
                }
                for attribute in element.attributes() {
                    let attribute = attribute?;
                    let value = attribute.decode_and_unescape_value(reader.decoder())?;
                    match attribute.key.local_name().as_ref() {
                        b"TargetMode" if value != "Internal" => {
                            return Err("external relationship is unsupported".into());
                        }
                        b"Target"
                            if value.contains(':')
                                || value.starts_with("//")
                                || value.contains('\\') =>
                        {
                            return Err("external relationship target is unsupported".into());
                        }
                        b"ContentType" | b"Type" => {
                            let lower = value.to_ascii_lowercase();
                            if [
                                "macroenabled",
                                "vbaproject",
                                "oleobject",
                                "activex",
                                "externallink",
                            ]
                            .iter()
                            .any(|part| lower.contains(part))
                            {
                                return Err("active content type is unsupported".into());
                            }
                        }
                        _ => {}
                    }
                }
            }
            Event::End(_) => {
                depth = depth.checked_sub(1).ok_or("unmatched XML end")?;
            }
            Event::Text(value) if depth == 0 && !value.decode()?.trim().is_empty() => {
                return Err("text outside XML root".into());
            }
            Event::Eof if depth != 0 || roots != 1 => {
                return Err("missing or truncated XML root".into());
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    fn package(name: &str, data: &[u8]) -> Vec<u8> {
        package_types(name, data, b"<Types/>")
    }
    fn package_types(name: &str, data: &[u8], types: &[u8]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (part, bytes) in [
            ("[Content_Types].xml", types),
            ("_rels/.rels", b"<Relationships/>".as_slice()),
            (name, data),
        ] {
            writer
                .start_file(part, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }
    #[test]
    fn preserves_inert_part_bytes_without_extracting() {
        let bytes = b"<slide><title>hello</title></slide>";
        assert_eq!(
            read(&package("ppt/slides/slide1.xml", bytes)).unwrap()["ppt/slides/slide1.xml"],
            bytes
        );
    }
    #[test]
    fn preserves_only_explicitly_typed_printer_settings_binary() {
        let name = "ppt/printerSettings/printerSettings1.bin";
        let types = "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"bin\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.printerSettings\"/></Types>";
        let payload = [0, 255, 0, 127];
        assert_eq!(
            read(&package_types(name, &payload, types.as_bytes())).unwrap()[name],
            payload
        );
        for changed in [types.replace("presentationml.printerSettings", "presentationml.other"), types.replace("2006/content-types", "2006/wrong"), types.replace("</Types>", "<Override PartName=\"/ppt/printerSettings/printerSettings1.bin\" ContentType=\"application/octet-stream\"/></Types>"), types.replace("</Types>", "<Default Extension=\"BIN\" ContentType=\"application/octet-stream\"/></Types>")] {
            assert!(read(&package_types(name, &payload, changed.as_bytes())).is_err());
        }
        assert!(
            read(&package_types(
                "ppt/vbaProject.bin",
                &payload,
                types.as_bytes()
            ))
            .is_err()
        );
        assert!(
            inspect_xml(b"<Relationship Type=\"http://example.invalid/vbaProject\"/>").is_err()
        );
    }
    #[test]
    fn rejects_paths_active_payloads_and_case_collisions() {
        for name in [
            "../escape.xml",
            "/absolute.xml",
            "ppt\\slide.xml",
            "ppt/%2e.xml",
            "PPT/../slide.xml",
            "ppt/vbaProject.bin",
            "ppt/embeddings/object.xml",
            "_RELS/.rels",
        ] {
            assert!(read(&package(name, b"<x/>")).is_err(), "{name}");
        }
    }
    #[test]
    fn rejects_external_entities_relationships_and_content_types() {
        for xml in [
            "<!DOCTYPE x SYSTEM 'https://example.invalid/a'><x/>",
            "<x TargetMode=\"External\" Target=\"https://example.invalid\"/>",
            "<x Target=\"https&#58;//example.invalid\"/>",
            "<x ContentType=\"application/vnd.ms-powerpoint.presentation.macroEnabled.main+xml\"/>",
            "<x></wrong>",
        ] {
            assert!(read(&package("ppt/part.xml", xml.as_bytes())).is_err());
        }
    }
    #[test]
    fn refuses_oversized_expansion() {
        assert!(read(&package("ppt/large.xml", &vec![b' '; MAX_PART + 1])).is_err());
    }

    #[test]
    fn rejects_missing_multiple_truncated_or_deep_xml_roots() {
        for xml in [
            String::new(),
            "<x/><y/>".into(),
            "<x>".into(),
            "text<x/>".into(),
            format!("{}{}", "<x>".repeat(129), "</x>".repeat(129)),
        ] {
            assert!(read(&package("ppt/part.xml", xml.as_bytes())).is_err());
        }
    }
}
