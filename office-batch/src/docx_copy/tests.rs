use super::*;
const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
fn package(document: &str) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in [
        (
            "[Content_Types].xml",
            "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Override PartName=\"/custom/body.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/></Types>",
        ),
        (
            "_rels/.rels",
            "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"doc\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"custom/body.xml\"/></Relationships>",
        ),
        ("custom/body.xml", document),
        ("custom/styles.xml", "<styles/>"),
    ] {
        writer
            .start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(bytes.as_bytes()).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

#[test]
fn replaces_body_using_relationships_and_preserves_section_and_other_parts() {
    let section = "<x:sectPr><x:pgSz x:w=\"100\" x:h=\"200\"/></x:sectPr>";
    let source = package(&format!(
        "<x:document xmlns:x=\"{W}\"><x:body><x:p><x:r><x:t>old</x:t></x:r></x:p>{section}</x:body></x:document>"
    ));
    let original = source.clone();
    let before = ooxml_package::read(&source).unwrap();
    let output = copy(
        &source,
        Action::ReplaceBodyText {
            text: "标题 & <tag>\t值\r\n\n结尾\n".into(),
        },
    )
    .unwrap();
    let after = ooxml_package::read(&output).unwrap();
    assert_eq!(source, original);
    assert_eq!(
        crate::docx_inspect::inspect(&source, 64).unwrap().body_text,
        "old"
    );
    assert_eq!(
        crate::docx_inspect::inspect(&output, 1024)
            .unwrap()
            .body_text,
        "标题 & <tag>\t值\n\n结尾\n"
    );
    for (name, value) in &before {
        if name != "custom/body.xml" {
            assert_eq!(after[name], *value, "{name}");
        }
    }
    let xml = std::str::from_utf8(&after["custom/body.xml"]).unwrap();
    assert!(!xml.contains(">old<"));
    assert!(xml.contains("标题 &amp; &lt;tag&gt;"));
    assert!(xml.contains("<w:tab/>"));
    assert_eq!(xml.matches("</w:p>").count(), 4);
    assert!(xml.contains(section));
    assert!(xml.starts_with(&format!("<x:document xmlns:x=\"{W}\"><x:body>")));
}

#[test]
fn handles_empty_body_and_rejects_ambiguous_structure_and_invalid_text() {
    let valid = package(&format!("<document xmlns=\"{W}\"><body/></document>"));
    let output = copy(
        &valid,
        Action::ReplaceBodyText {
            text: String::new(),
        },
    )
    .unwrap();
    let parts = ooxml_package::read(&output).unwrap();
    assert!(
        std::str::from_utf8(&parts["custom/body.xml"])
            .unwrap()
            .contains("</w:p></body>")
    );
    for body in [
        "<x:body/><x:body/>",
        "<x:body><x:sectPr/><x:p/></x:body>",
        "<x:other/>",
    ] {
        let source = package(&format!("<x:document xmlns:x=\"{W}\">{body}</x:document>"));
        assert!(copy(&source, Action::ReplaceBodyText { text: "new".into() }).is_err());
    }
    for text in ["bad\0text".into(), "bad\u{ffff}".into(), "x".repeat(65537)] {
        assert!(copy(&valid, Action::ReplaceBodyText { text }).is_err());
    }
    assert!(crate::docx_parts::validate_main_type(b"<Types/>", "custom/body.xml").is_err());
}

#[test]
fn observation_is_bounded_and_does_not_guess_revision_or_alternate_content() {
    let document = |body: &str| {
        package(&format!(
            "<x:document xmlns:x=\"{W}\"><x:body>{body}</x:body></x:document>"
        ))
    };
    let source = document(
        "<x:p><x:r><x:t>中&amp;文</x:t><x:tab/><x:t>值</x:t><x:br/><x:t>end</x:t></x:r></x:p><x:p/>",
    );
    let expected = "中&文\t值\nend\n";
    assert_eq!(
        crate::docx_inspect::inspect(&source, expected.len())
            .unwrap()
            .body_text,
        expected
    );
    assert!(crate::docx_inspect::inspect(&source, expected.len() - 1).is_err());
    assert!(crate::docx_inspect::inspect(&source, 0).is_err());
    for body in [
        "<x:p><x:ins><x:r><x:t>pending change</x:t></x:r></x:ins></x:p>",
        "<x:altChunk/>",
        "<x:p><x:p/></x:p>",
        "<mc:AlternateContent xmlns:mc=\"http://schemas.openxmlformats.org/markup-compatibility/2006\"><mc:Choice/><mc:Fallback/></mc:AlternateContent>",
    ] {
        let source = document(body);
        assert!(
            crate::docx_inspect::inspect(&source, 1024).is_err(),
            "{body}"
        );
        assert!(
            copy(&source, Action::ReplaceBodyText { text: "new".into() }).is_err(),
            "{body}"
        );
    }
}
