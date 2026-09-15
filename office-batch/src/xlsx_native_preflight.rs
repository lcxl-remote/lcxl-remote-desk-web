//! Closed package/relationship admission before opening a private copy in Excel.
//! This is file-content admission only; the runner still owns OS identity,
//! instance isolation, host policies, deadlines and current-run result checks.
use crate::{
    ooxml_package::{self, PackageResult},
    ooxml_relationships::{self as rel, Parts, attributes},
    xlsx_formula_inventory,
};
use quick_xml::{NsReader, events::Event, name::ResolveResult};
use std::collections::{BTreeMap, HashSet, VecDeque};

const CONTENT_TYPES: &[u8] = b"http://schemas.openxmlformats.org/package/2006/content-types";
const OFFICE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/";
const MIME: &str = "application/vnd.openxmlformats-officedocument.";

pub fn inspect(bytes: &[u8]) -> PackageResult<xlsx_formula_inventory::Inventory> {
    let parts = ooxml_package::read(bytes)?;
    inspect_xml_payloads(&parts)?;
    let types = types(&parts)?;
    let mut seen = HashSet::new();
    seen.insert("[Content_Types].xml".to_owned());
    let mut sources = VecDeque::from([String::new()]);
    while let Some(source) = sources.pop_front() {
        let relationships = rel::relationship_path(&source);
        if !parts.contains_key(&relationships) {
            continue;
        }
        if !seen.insert(relationships.clone()) {
            continue;
        }
        for relationship in rel::relationships(&parts, &source)? {
            let target = rel::target(&parts, &source, &relationship.target)?;
            let mime = types
                .get(&target)
                .ok_or("native Excel relationship target has no content type")?;
            if !allowed(&relationship.kind, mime) {
                return Err(
                    "native Excel package has an unsupported relationship or content type".into(),
                );
            }
            if seen.insert(target.clone()) {
                sources.push_back(target);
            }
        }
    }
    // No undeclared payload or unvisited relationship can become active only
    // after Excel discovers it. Standard images and chart/drawing parts are
    // reachable through their own closed relationship types.
    if parts.keys().any(|name| !seen.contains(name)) {
        return Err("native Excel package contains unreferenced parts".into());
    }
    xlsx_formula_inventory::inspect(bytes)
}

fn inspect_xml_payloads(parts: &Parts) -> PackageResult<()> {
    let sheets = crate::xlsx_parts::sheet_names(parts)?;
    for (path, bytes) in parts.iter().filter(|(path, _)| path.ends_with(".xml")) {
        let mut reader = NsReader::from_reader(bytes.as_slice());
        reader.config_mut().expand_empty_elements = true;
        let mut chart_formula: Option<String> = None;
        loop {
            let (ns, event) = reader.read_resolved_event()?;
            match event {
                Event::Start(element) => {
                    if chart_formula.is_some() {
                        return Err("nested chart formula is unsupported".into());
                    }
                    let local = element.local_name();
                    if matches!(local.as_ref(), b"extLst" | b"AlternateContent") {
                        return Err(
                            "native Excel extension payload requires a supported adapter".into(),
                        );
                    }
                    if local.as_ref() == b"f"
                        && matches!(ns, ResolveResult::Bound(ref ns) if ns.as_ref() == b"http://schemas.openxmlformats.org/drawingml/2006/chart")
                    {
                        chart_formula = Some(String::new());
                    }
                }
                Event::Text(_) | Event::CData(_) | Event::GeneralRef(_)
                    if chart_formula.is_some() =>
                {
                    let value = chart_formula.as_mut().unwrap();
                    value.push_str(&crate::xlsx_strings::event_text(event)?);
                    if value.len() >= 4096 {
                        return Err("chart formula exceeds bounds".into());
                    }
                }
                Event::End(_) => {
                    if let Some(formula) = chart_formula.take() {
                        desk_diagnose_core::spreadsheet_formula::validate_formula_patch(
                            &format!("={formula}"),
                            "A1",
                            desk_diagnose_core::spreadsheet_formula::FORMULA_LOCALE_V1,
                            &sheets,
                        )
                        .map_err(|_| format!("unsupported native chart reference in {path}"))?;
                    }
                }
                Event::PI(_) => {
                    return Err("native Excel XML processing instructions are unsupported".into());
                }
                Event::Eof => break,
                _ => {}
            }
        }
    }
    Ok(())
}

fn allowed(kind: &str, mime: &str) -> bool {
    if kind
        == "http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties"
    {
        return mime == "application/vnd.openxmlformats-package.core-properties+xml";
    }
    let Some(kind) = kind.strip_prefix(OFFICE) else {
        return false;
    };
    let expected = match kind {
        "officeDocument" => "spreadsheetml.sheet.main+xml",
        "worksheet" => "spreadsheetml.worksheet+xml",
        "styles" => "spreadsheetml.styles+xml",
        "sharedStrings" => "spreadsheetml.sharedStrings+xml",
        "calcChain" => "spreadsheetml.calcChain+xml",
        "table" => "spreadsheetml.table+xml",
        "theme" => "theme+xml",
        "drawing" => "drawing+xml",
        "chart" => "drawingml.chart+xml",
        "comments" => "spreadsheetml.comments+xml",
        "extended-properties" => "extended-properties+xml",
        "custom-properties" => "custom-properties+xml",
        "image" => return matches!(mime, "image/png" | "image/jpeg" | "image/gif"),
        _ => return false,
    };
    mime.strip_prefix(MIME) == Some(expected)
}

fn types(parts: &Parts) -> PackageResult<BTreeMap<String, String>> {
    let mut reader = NsReader::from_reader(parts["[Content_Types].xml"].as_slice());
    reader.config_mut().expand_empty_elements = true;
    let mut defaults = BTreeMap::new();
    let mut overrides = BTreeMap::new();
    let mut depth = 0usize;
    loop {
        let (ns, event) = reader.read_resolved_event()?;
        match event {
            Event::Start(element) => {
                depth += 1;
                if !matches!(ns, ResolveResult::Bound(ref ns) if ns.as_ref() == CONTENT_TYPES) {
                    return Err("unsupported content type namespace".into());
                }
                let attrs = attributes(&reader, &element)?;
                let value = |key: &str| {
                    attrs
                        .get(key)
                        .filter(|v| !v.is_empty())
                        .cloned()
                        .ok_or("missing content type attribute")
                };
                match (depth, element.local_name().as_ref()) {
                    (1, b"Types") => {}
                    (2, b"Default") => {
                        if defaults
                            .insert(
                                value("Extension")?.to_ascii_lowercase(),
                                value("ContentType")?,
                            )
                            .is_some()
                        {
                            return Err("duplicate default content type".into());
                        }
                    }
                    (2, b"Override") => {
                        let name = value("PartName")?;
                        let name = name
                            .strip_prefix('/')
                            .ok_or("invalid part content type name")?;
                        if !parts.contains_key(name)
                            || overrides
                                .insert(name.to_owned(), value("ContentType")?)
                                .is_some()
                        {
                            return Err("ambiguous content type override".into());
                        }
                    }
                    _ => return Err("unsupported content type structure".into()),
                }
            }
            Event::End(_) => depth -= 1,
            Event::Eof => break,
            _ => {}
        }
    }
    let mut resolved = BTreeMap::new();
    for name in parts
        .keys()
        .filter(|name| name.as_str() != "[Content_Types].xml")
    {
        let ext = name
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .unwrap_or_default();
        let value = overrides
            .get(name)
            .or_else(|| defaults.get(&ext))
            .ok_or("untyped native Excel part")?;
        if ext == "rels" && value != "application/vnd.openxmlformats-package.relationships+xml" {
            return Err("invalid relationship content type".into());
        }
        resolved.insert(name.clone(), value.clone());
    }
    Ok(resolved)
}
