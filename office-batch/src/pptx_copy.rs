//! Copy-only package edit; publication and source authority belong to the host.
use crate::{
    ooxml_package::{self, PackageResult},
    pptx_notes, pptx_parts, pptx_text,
};
use std::io::{Cursor, Write};

pub use desk_agent_protocol::computer_use::PresentationLivePatchAction as Action;

pub fn copy(bytes: &[u8], slide_number: usize, action: Action) -> PackageResult<Vec<u8>> {
    let mut parts = ooxml_package::read(bytes)?;
    let selected = pptx_parts::locate(&parts, slide_number)?;
    let (part, title, text) = match action {
        Action::ReplaceSlideTitle { text } => (selected.slide, true, text),
        Action::SetPresenterNotes { text } => (
            selected.notes.ok_or("selected slide has no notes part")?,
            false,
            text,
        ),
    };
    let edited = if title {
        pptx_text::replace(&parts[&part], true, &text)?
    } else {
        pptx_notes::replace(&parts[&part], &text)?
    };
    parts.insert(part, edited);
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in &parts {
        writer.start_file(name, zip::write::SimpleFileOptions::default())?;
        writer.write_all(bytes)?;
    }
    let output = writer.finish()?.into_inner();
    if ooxml_package::read(&output)? != parts {
        return Err("copy package readback mismatch".into());
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn shape(kind: &str) -> String {
        format!(
            "<p:sld xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\" xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:nvPr><p:ph type=\"{kind}\"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>original</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"
        )
    }
    fn fixture() -> Vec<u8> {
        let rels = |kind: &str, target: &str| {
            format!(
                "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"id\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/{kind}\" Target=\"{target}\"/></Relationships>"
            )
        };
        let parts = [
            ("[Content_Types].xml", "<Types/>".into()), ("_rels/.rels", rels("officeDocument", "ppt/pres.xml")),
            ("ppt/pres.xml", "<p:presentation xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><p:sldIdLst><p:sldId id=\"256\" r:id=\"id\"/></p:sldIdLst></p:presentation>".into()),
            ("ppt/_rels/pres.xml.rels", rels("slide", "slides/custom.xml")),
            ("ppt/slides/custom.xml", shape("title")),
            ("ppt/slides/_rels/custom.xml.rels", rels("notesSlide", "../notes/custom.xml")),
            ("ppt/notes/custom.xml", shape("body")), ("ppt/media/keep.png", "unaltered test payload".into()),
        ];
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, text) in parts {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(text.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }
    #[test]
    fn each_action_changes_only_its_own_part() {
        let input = fixture();
        let before = ooxml_package::read(&input).unwrap();
        for (action, changed_part) in [
            (
                Action::ReplaceSlideTitle {
                    text: "title only".into(),
                },
                "ppt/slides/custom.xml",
            ),
            (
                Action::SetPresenterNotes {
                    text: "notes only".into(),
                },
                "ppt/notes/custom.xml",
            ),
        ] {
            let output = copy(&input, 1, action).unwrap();
            let after = ooxml_package::read(&output).unwrap();
            let changed: Vec<_> = before
                .iter()
                .filter_map(|(name, bytes)| (after[name] != *bytes).then_some(name.as_str()))
                .collect();
            assert_eq!(changed, [changed_part]);
        }
    }

    #[test]
    fn composed_copy_changes_only_title_and_notes_and_keeps_input_bytes() {
        let input = fixture();
        let original = input.clone();
        let before = ooxml_package::read(&input).unwrap();
        let titled = copy(
            &input,
            1,
            Action::ReplaceSlideTitle {
                text: "标题 & more".into(),
            },
        )
        .unwrap();
        let output = copy(
            &titled,
            1,
            Action::SetPresenterNotes {
                text: "备注".into(),
            },
        )
        .unwrap();
        let after = ooxml_package::read(&output).unwrap();
        for (name, bytes) in &before {
            let expected = match name.as_str() {
                "ppt/slides/custom.xml" => String::from_utf8(bytes.clone())
                    .unwrap()
                    .replace(">original<", ">标题 &amp; more<")
                    .into_bytes(),
                "ppt/notes/custom.xml" => pptx_notes::replace(bytes, "备注").unwrap(),
                _ => bytes.clone(),
            };
            assert_eq!(after[name], expected, "{name}");
        }
        assert_eq!(input, original);
        assert!(
            copy(
                &input,
                1,
                Action::SetPresenterNotes {
                    text: "invalid\0notes".into()
                }
            )
            .is_err()
        );
        assert_eq!(input, original);
    }
}
