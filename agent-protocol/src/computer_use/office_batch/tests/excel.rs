use super::*;

fn excel_draft(name: &str) -> ComputerActionDraft {
    let mut draft = draft();
    draft.adapter = ComputerUseAdapterRef {
        kind: ComputerUseAdapterKind::OfficeExcel,
        version: XLSX_ADAPTER_VERSION.into(),
    };
    draft.actions[0].target.object_kind = ObjectKind::Range;
    let ComputerActionKind::PresentationLiveBatch(batch) = &draft.actions[0].action else {
        unreachable!()
    };
    let mut output = batch.output.clone();
    output.native_file_name = name.into();
    draft.actions[0].action =
        ComputerActionKind::SpreadsheetLiveBatch(SpreadsheetLiveBatchPatchAction {
            output,
            action: SpreadsheetLivePatchAction::SetCellValue { value: "42".into() },
        });
    draft
}

#[test]
fn excel_batch_accepts_only_exact_version_and_file_actions() {
    let good = excel_draft("reviewed.XLSX");
    good.validate().unwrap();
    for (kind, version) in [
        (ComputerUseAdapterKind::OfficeExcel, "office-xlsx-batch/v2"),
        (ComputerUseAdapterKind::IworkNumbers, XLSX_ADAPTER_VERSION),
        (ComputerUseAdapterKind::OfficeWord, XLSX_ADAPTER_VERSION),
        (
            ComputerUseAdapterKind::OfficeExcel,
            "office-js-bridge-read/v1",
        ),
    ] {
        let mut bad = good.clone();
        bad.adapter = ComputerUseAdapterRef {
            kind,
            version: version.into(),
        };
        assert!(bad.validate().is_err());
    }
    for action in [
        ComputerActionKind::Excel(ExcelPatchAction::SetValue { value: "42".into() }),
        ComputerActionKind::SpreadsheetLive(SpreadsheetLivePatchAction::SetCellValue {
            value: "42".into(),
        }),
    ] {
        let mut bad = good.clone();
        bad.actions[0].action = action;
        assert!(bad.validate().is_err());
    }
    let mut formula = good.clone();
    let ComputerActionKind::SpreadsheetLiveBatch(batch) = &mut formula.actions[0].action else {
        unreachable!()
    };
    batch.action = SpreadsheetLivePatchAction::SetCellFormula {
        formula: "=A1*2".into(),
    };
    formula.validate().unwrap();
    let mut wrong_target = good;
    wrong_target.actions[0].target.object_kind = ObjectKind::File;
    assert!(wrong_target.validate().is_err());
}

#[test]
fn excel_output_contract_supports_safe_255_byte_names_and_preserves_artifact_binding() {
    use crate::data_lineage::ContentRef;
    for (name, valid) in [
        (format!("{}.xlsx", "a".repeat(250)), true),
        (format!("{}.XLSX", "中".repeat(83)), true),
        (format!("{}.xlsx", "a".repeat(251)), false),
        ("../copy.xlsx".into(), false),
        ("NUL.xlsx".into(), false),
        ("x:stream.xlsx".into(), false),
        ("copy.numbers".into(), false),
        ("copy.xlsm".into(), false),
    ] {
        assert_eq!(excel_draft(&name).validate().is_ok(), valid, "{name}");
        assert_eq!(valid_xlsx_leaf(&name), valid);
        if !valid {
            continue;
        }
        let mut receipt = CreatedFileArtifactOutput {
            file: ObjectRef {
                token: "created".into(),
                object_kind: ObjectKind::File,
                snapshot_id: "worker:1".into(),
                expires_at: "2030-01-01T00:00:00Z".into(),
            },
            file_name: name,
            media_type: XLSX_MEDIA_TYPE.into(),
            size_bytes: 10,
            digest_sha256: "a".repeat(64),
            content: ContentRef::Artifact {
                artifact_id: "created".into(),
                sha256: "a".repeat(64),
                size_bytes: 10,
                media_type: XLSX_MEDIA_TYPE.into(),
            },
        };
        receipt.validate().unwrap();
        receipt.digest_sha256 = "b".repeat(64);
        assert!(receipt.validate().is_err());
    }
}
