//! Fail closed when native saving changes nonformula content or formatting.
//! ZIP encoding, XML prefixes/attribute order, calculation metadata, chart caches
//! and normal modification timestamps may differ. Cell values/styles may not.
use crate::{
    ooxml_package::{self, PackageResult},
    ooxml_relationships as rel, xlsx_formula_inventory, xlsx_parts, xlsx_strings,
};
use quick_xml::{NsReader, events::Event, name::ResolveResult};
use std::collections::{BTreeMap, BTreeSet, HashSet};
const S: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const CHAIN: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/calcChain";
const CHAIN_MIME: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.calcChain+xml";

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Start(String, String, Vec<(String, String, String)>),
    Text(String),
    End,
}

pub fn validate(prepared: &[u8], saved: &[u8]) -> PackageResult<()> {
    let before = ooxml_package::read(prepared)?;
    let after = ooxml_package::read(saved)?;
    let inventory = xlsx_formula_inventory::inspect(prepared)?;
    let mut formula_cells: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    for formula in inventory.formulas {
        let selection = xlsx_parts::locate(&before, &formula.sheet)?;
        formula_cells
            .entry(selection.worksheet)
            .or_default()
            .insert(formula.address);
    }
    let chain = |parts: &rel::Parts| -> PackageResult<Option<String>> {
        let main = rel::main_part(parts)?;
        let links = rel::relationships_optional(parts, &main)?;
        rel::unique(&links, CHAIN)?
            .map(|r| rel::target(parts, &main, &r.target))
            .transpose()
    };
    let before_chain = chain(&before)?;
    let after_chain = chain(&after)?;
    let before_names: BTreeSet<_> = before
        .keys()
        .filter(|name| Some(*name) != before_chain.as_ref())
        .collect();
    let after_names: BTreeSet<_> = after
        .keys()
        .filter(|name| Some(*name) != after_chain.as_ref())
        .collect();
    if before_names != after_names {
        return Err("native Excel saving added or removed non-calculation parts".into());
    }
    let empty = HashSet::new();
    for name in before_names {
        if before[name] == after[name] {
            continue;
        }
        if !(name.ends_with(".xml") || name.ends_with(".rels")) {
            if before[name] != after[name] {
                return Err("native Excel changed a binary or media part".into());
            }
            continue;
        }
        let cells = formula_cells.get(name).unwrap_or(&empty);
        if canonical(&before[name], cells)? != canonical(&after[name], cells)? {
            return Err(format!(
                "native Excel changed nonformula content, metadata or formatting in {name}"
            )
            .into());
        }
    }
    Ok(())
}

fn namespace(ns: ResolveResult<'_>) -> PackageResult<String> {
    match ns {
        ResolveResult::Bound(ns) => Ok(std::str::from_utf8(ns.as_ref())?.into()),
        ResolveResult::Unbound => Ok(String::new()),
        ResolveResult::Unknown(_) => Err("unresolved XML namespace".into()),
    }
}
fn canonical(xml: &[u8], formulas: &HashSet<String>) -> PackageResult<Vec<Token>> {
    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().expand_empty_elements = true;
    let mut result = Vec::new();
    let mut stack: Vec<(String, String)> = Vec::new();
    let mut ignored = None;
    let mut formula_cell = None;
    loop {
        let (ns, event) = reader.read_resolved_event()?;
        match event {
            Event::Start(element) => {
                let ns = namespace(ns)?;
                let name = std::str::from_utf8(element.local_name().as_ref())?.to_owned();
                let mut attrs = Vec::new();
                for attribute in element.attributes() {
                    let attribute = attribute?;
                    if attribute.key.as_ref() == b"xmlns"
                        || attribute.key.as_ref().starts_with(b"xmlns:")
                    {
                        continue;
                    }
                    let (attribute_ns, key) = reader.resolve_attribute(attribute.key);
                    let attribute_ns = namespace(attribute_ns)?;
                    let key = std::str::from_utf8(key.as_ref())?.to_owned();
                    if attribute_ns == "http://schemas.openxmlformats.org/markup-compatibility/2006"
                        && key == "Ignorable"
                    {
                        continue;
                    }
                    attrs.push((
                        attribute_ns,
                        key,
                        attribute
                            .decode_and_unescape_value(reader.decoder())?
                            .into_owned(),
                    ));
                }
                let attr = |key: &str| {
                    attrs
                        .iter()
                        .find(|(ns, k, _)| ns.is_empty() && k == key)
                        .map(|(_, _, v)| v.as_str())
                };
                if ns == S && name == "c" && attr("r").is_some_and(|r| formulas.contains(r)) {
                    formula_cell = Some(stack.len());
                }
                let skip = (ns == S && name == "calcPr")
                    || (ns == "http://schemas.openxmlformats.org/drawingml/2006/chart"
                        && matches!(name.as_str(), "numCache" | "strCache"))
                    || (formula_cell.is_some() && ns == S && matches!(name.as_str(), "f" | "v"))
                    || (ns == "http://schemas.openxmlformats.org/package/2006/relationships"
                        && name == "Relationship"
                        && attr("Type") == Some(CHAIN))
                    || (ns == "http://schemas.openxmlformats.org/package/2006/content-types"
                        && name == "Override"
                        && attr("ContentType") == Some(CHAIN_MIME))
                    || (ns == "http://purl.org/dc/terms/" && name == "modified")
                    || (ns
                        == "http://schemas.openxmlformats.org/package/2006/metadata/core-properties"
                        && matches!(name.as_str(), "lastModifiedBy" | "revision"));
                if skip && ignored.is_none() {
                    ignored = Some(stack.len());
                }
                if ignored.is_none() {
                    if formula_cell == Some(stack.len()) {
                        attrs.retain(|(ns, name, _)| !ns.is_empty() || name != "t");
                    }
                    attrs.sort();
                    result.push(Token::Start(ns.clone(), name.clone(), attrs));
                }
                stack.push((ns, name));
            }
            Event::End(_) => {
                stack.pop().ok_or("invalid XML nesting")?;
                if ignored == Some(stack.len()) {
                    ignored = None;
                } else if ignored.is_none() {
                    result.push(Token::End);
                }
                if formula_cell == Some(stack.len()) {
                    formula_cell = None;
                }
            }
            Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) if ignored.is_none() => {
                let text = xlsx_strings::event_text(event)?;
                let preserve_space = stack
                    .last()
                    .is_some_and(|(ns, name)| ns == S && name == "t");
                if !preserve_space && text.trim().is_empty() {
                    continue;
                }
                if let Some(Token::Text(previous)) = result.last_mut() {
                    previous.push_str(&text);
                } else {
                    result.push(Token::Text(text));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err("incomplete preservation XML".into());
    }
    Ok(result)
}
