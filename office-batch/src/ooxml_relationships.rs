//! Package relationship resolution shared by typed Office file adapters.
use crate::ooxml_package::PackageResult;
use quick_xml::{
    NsReader,
    events::{BytesStart, Event},
    name::ResolveResult,
};
use std::collections::{BTreeMap, HashSet};
pub(crate) const PKG: &[u8] = b"http://schemas.openxmlformats.org/package/2006/relationships";
pub(crate) const OFFICE: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";
pub(crate) type Parts = BTreeMap<String, Vec<u8>>;
pub(crate) struct Relationship {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) target: String,
}
pub(crate) fn main_part(parts: &Parts) -> PackageResult<String> {
    let roots = relationships(parts, "")?;
    let root = unique(&roots, OFFICE)?.ok_or("missing office document relationship")?;
    target(parts, "", &root.target)
}
pub(crate) fn unique<'a>(
    links: &'a [Relationship],
    kind: &str,
) -> PackageResult<Option<&'a Relationship>> {
    let mut matches = links.iter().filter(|link| link.kind == kind);
    let first = matches.next();
    if matches.next().is_some() {
        return Err("ambiguous relationship type".into());
    }
    Ok(first)
}
pub(crate) fn xml<'a>(parts: &'a Parts, path: &str) -> PackageResult<&'a str> {
    Ok(std::str::from_utf8(
        parts.get(path).ok_or("missing XML part")?,
    )?)
}
pub(crate) fn relationship_path(source: &str) -> String {
    match source.rsplit_once('/') {
        Some((directory, leaf)) => format!("{directory}/_rels/{leaf}.rels"),
        None => format!("_rels/{source}.rels"),
    }
}
pub(crate) fn relationships_optional(
    parts: &Parts,
    source: &str,
) -> PackageResult<Vec<Relationship>> {
    if !parts.contains_key(&relationship_path(source)) {
        return Ok(vec![]);
    }
    relationships(parts, source)
}
pub(crate) fn relationships(parts: &Parts, source: &str) -> PackageResult<Vec<Relationship>> {
    let mut reader = NsReader::from_str(xml(parts, &relationship_path(source))?);
    reader.config_mut().expand_empty_elements = true;
    let mut depth = 0;
    let mut seen = HashSet::new();
    let mut links = Vec::new();
    loop {
        let (namespace, event) = reader.read_resolved_event()?;
        match event {
            Event::Start(element) => {
                depth += 1;
                if !matches!(namespace, ResolveResult::Bound(ref ns) if ns.as_ref() == PKG)
                    || !matches!(
                        (depth, element.local_name().as_ref()),
                        (1, b"Relationships") | (2, b"Relationship")
                    )
                {
                    return Err("unexpected relationship XML structure".into());
                }
                if depth == 2 {
                    let attrs = attributes(&reader, &element)?;
                    let get = |name: &str| {
                        attrs
                            .get(name)
                            .filter(|value| !value.is_empty())
                            .cloned()
                            .ok_or("missing relationship attribute")
                    };
                    let id = get("Id")?;
                    if !seen.insert(id.clone())
                        || attrs
                            .get("TargetMode")
                            .is_some_and(|mode| mode != "Internal")
                    {
                        return Err("ambiguous or external relationship".into());
                    }
                    links.push(Relationship {
                        id,
                        kind: get("Type")?,
                        target: get("Target")?,
                    });
                }
            }
            Event::End(_) => depth -= 1,
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(links)
}
pub(crate) fn attributes(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
) -> PackageResult<BTreeMap<String, String>> {
    let mut attrs = BTreeMap::new();
    for attribute in element.attributes() {
        let attribute = attribute?;
        attrs.insert(
            std::str::from_utf8(attribute.key.as_ref())?.into(),
            attribute
                .decode_and_unescape_value(reader.decoder())?
                .into_owned(),
        );
    }
    Ok(attrs)
}
pub(crate) fn target(parts: &Parts, source: &str, relative: &str) -> PackageResult<String> {
    if relative.is_empty()
        || relative.starts_with("//")
        || relative
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, ':' | '\\' | '%' | '#' | '?'))
    {
        return Err("unsupported relationship target".into());
    }
    let mut components: Vec<&str> = if relative.starts_with('/') {
        vec![]
    } else {
        source
            .rsplit_once('/')
            .map(|(directory, _)| directory.split('/').collect())
            .unwrap_or_default()
    };
    for part in relative.strip_prefix('/').unwrap_or(relative).split('/') {
        match part {
            ".." => {
                components
                    .pop()
                    .ok_or("relationship escapes package root")?;
            }
            "." => {}
            "" => return Err("empty relationship path segment".into()),
            part => components.push(part),
        }
    }
    let path = components.join("/");
    if !parts.contains_key(&path) {
        return Err("relationship target does not exist".into());
    }
    Ok(path)
}
