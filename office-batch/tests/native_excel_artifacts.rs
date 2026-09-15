//! Reads artifacts from plans/validation/verify-windows-excel-native.py.
//! This proves the production validators accept a real Excel save, not merely
//! that another spreadsheet reader can open it. No Office activation occurs.
use desk_office_batch::{
    xlsx_native_preservation,
    xlsx_result::Scalar,
    xlsx_workbook_result::{self, CalculationReadback},
};

#[test]
#[ignore = "requires ASSISTANT_EXCEL_ARTIFACT_DIR from the native validation script"]
fn real_excel_save_preserves_calculation_inputs() {
    let directory = std::path::PathBuf::from(
        std::env::var_os("ASSISTANT_EXCEL_ARTIFACT_DIR").expect("native artifact directory"),
    );
    let before = std::fs::read(directory.join("source.xlsx")).unwrap();
    let after = std::fs::read(directory.join("calculated.xlsx")).unwrap();
    desk_office_batch::xlsx_native_inputs::validate(&before, &after).unwrap();
}

#[test]
#[ignore = "requires ASSISTANT_EXCEL_ARTIFACT_DIR from the native validation script"]
fn real_excel_save_passes_publication_validators() {
    let directory = std::path::PathBuf::from(
        std::env::var_os("ASSISTANT_EXCEL_ARTIFACT_DIR").expect("native artifact directory"),
    );
    let before = std::fs::read(directory.join("source.xlsx")).unwrap();
    let after = std::fs::read(directory.join("calculated.xlsx")).unwrap();
    let readback = [
        ("B1", Scalar::Number(42.0)),
        ("C1", Scalar::Text("ready".into())),
        ("D1", Scalar::Boolean(true)),
    ]
    .into_iter()
    .map(|(address, value)| CalculationReadback {
        sheet: "数据".into(),
        address: address.into(),
        value,
    })
    .collect::<Vec<_>>();
    xlsx_workbook_result::compare(&before, &after, &readback).expect("native cache proof");
    let merged = desk_office_batch::xlsx_native_merge::merge(&before, &after, &readback)
        .expect("original package cache merge");
    xlsx_native_preservation::validate(&before, &merged).expect("nonformula preservation proof");
    xlsx_workbook_result::compare(&before, &merged, &readback).expect("merged cache proof");
    let original_parts = desk_office_batch::ooxml_package::read(&before).unwrap();
    let merged_parts = desk_office_batch::ooxml_package::read(&merged).unwrap();
    assert_eq!(
        original_parts.keys().collect::<Vec<_>>(),
        merged_parts.keys().collect::<Vec<_>>()
    );
    for (name, bytes) in &original_parts {
        if name != "xl/worksheets/sheet1.xml" {
            assert_eq!(&merged_parts[name], bytes, "changed part: {name}");
        }
    }
    std::fs::write(directory.join("merged.xlsx"), merged).unwrap();
}
