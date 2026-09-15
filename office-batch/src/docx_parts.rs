//! Locate a standard Word document through its package relationship and MIME type.
use crate::{
    ooxml_package::PackageResult,
    ooxml_relationships::{self, Parts},
};
pub(crate) fn locate(parts: &Parts) -> PackageResult<String> {
    let main = ooxml_relationships::main_part(parts)?;
    validate_main_type(&parts["[Content_Types].xml"], &main)?;
    Ok(main)
}
pub(crate) fn validate_main_type(bytes: &[u8], main: &str) -> PackageResult<()> {
    use quick_xml::{NsReader, events::Event, name::ResolveResult};
    let mut reader = NsReader::from_str(std::str::from_utf8(bytes)?);
    let mut found = false;
    loop {
        let (ns, event) = reader.read_resolved_event()?;
        match event {
            Event::Start(element) | Event::Empty(element)
                if matches!(ns, ResolveResult::Bound(ref ns) if ns.as_ref() == b"http://schemas.openxmlformats.org/package/2006/content-types")
                    && element.local_name().as_ref() == b"Override" =>
            {
                let mut name = None;
                let mut mime = None;
                for attr in element.attributes() {
                    let attr = attr?;
                    let value = attr
                        .decode_and_unescape_value(reader.decoder())?
                        .into_owned();
                    match attr.key.as_ref() {
                        b"PartName" => name = Some(value),
                        b"ContentType" => mime = Some(value),
                        _ => {}
                    }
                }
                if name.as_deref() == Some(format!("/{main}").as_str()) {
                    if found
                        || mime.as_deref()
                            != Some(
                                "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
                            )
                    {
                        return Err("wrong or duplicate Word main content type".into());
                    }
                    found = true;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !found {
        return Err("missing Word main content type".into());
    }
    Ok(())
}
