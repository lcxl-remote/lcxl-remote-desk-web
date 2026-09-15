//! Typed Word copy transformation. The caller owns selection, approval and publication.
use crate::{
    docx_body, docx_parts,
    ooxml_package::{self, PackageResult},
};
pub use desk_agent_protocol::computer_use::DocumentLivePatchAction as Action;
use std::io::{Cursor, Write};

pub fn copy(bytes: &[u8], action: Action) -> PackageResult<Vec<u8>> {
    let mut parts = ooxml_package::read(bytes)?;
    let main = docx_parts::locate(&parts)?;
    let limit = desk_agent_protocol::computer_use::MAX_LIVE_DOCUMENT_TEXT_BYTES;
    crate::docx_inspect::body_text(&parts[&main], limit)?;

    let Action::ReplaceBodyText { text } = action;
    let edited = docx_body::replace(&parts[&main], &text)?;
    if crate::docx_inspect::body_text(&edited, limit)?
        != text.replace("\r\n", "\n").replace('\r', "\n")
    {
        return Err("document copy body text readback mismatch".into());
    }
    parts.insert(main, edited);
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in &parts {
        writer.start_file(name, zip::write::SimpleFileOptions::default())?;
        writer.write_all(bytes)?;
    }
    let output = writer.finish()?.into_inner();
    if ooxml_package::read(&output)? != parts {
        return Err("document copy package readback mismatch".into());
    }
    Ok(output)
}

#[cfg(test)]
mod tests;
