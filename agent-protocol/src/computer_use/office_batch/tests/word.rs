use super::*;

fn word_draft(name: &str) -> ComputerActionDraft {
    let mut draft = draft();
    draft.adapter = ComputerUseAdapterRef {
        kind: ComputerUseAdapterKind::OfficeWord,
        version: DOCX_ADAPTER_VERSION.into(),
    };
    draft.actions[0].target.object_kind = ObjectKind::Document;
    let ComputerActionKind::PresentationLiveBatch(batch) = &draft.actions[0].action else {
        unreachable!()
    };
    let mut output = batch.output.clone();
    output.native_file_name = name.into();
    draft.actions[0].action = ComputerActionKind::DocumentLiveBatch(DocumentLiveBatchPatchAction {
        output,
        action: DocumentLivePatchAction::ReplaceBodyText {
            text: "Reviewed body".into(),
        },
    });
    draft
}

#[test]
fn word_batch_accepts_only_its_exact_adapter_and_copy_action() {
    let good = word_draft("reviewed.DOCX");
    good.validate().unwrap();
    for (kind, version) in [
        (ComputerUseAdapterKind::OfficeWord, "office-docx-batch/v2"),
        (ComputerUseAdapterKind::OfficeWord, "live/v1"),
        (ComputerUseAdapterKind::IworkPages, DOCX_ADAPTER_VERSION),
        (
            ComputerUseAdapterKind::OfficePowerPoint,
            DOCX_ADAPTER_VERSION,
        ),
    ] {
        let mut bad = good.clone();
        bad.adapter = ComputerUseAdapterRef {
            kind,
            version: version.into(),
        };
        assert!(bad.validate().is_err());
    }
    let mut live = good.clone();
    live.actions[0].action =
        ComputerActionKind::DocumentLive(DocumentLivePatchAction::ReplaceBodyText {
            text: "live".into(),
        });
    assert!(live.validate().is_err());
    let mut wrong_target = good;
    wrong_target.actions[0].target.object_kind = ObjectKind::Slide;
    assert!(wrong_target.validate().is_err());
}

#[test]
fn word_output_name_and_receipt_share_limits() {
    use crate::data_lineage::ContentRef;
    for (name, valid) in [
        (format!("{}.docx", "a".repeat(250)), true),
        (format!("{}.DOCX", "中".repeat(83)), true),
        (format!("{}.docx", "a".repeat(251)), false),
        ("../copy.docx".into(), false),
        ("NUL.docx".into(), false),
        ("copy.pages".into(), false),
    ] {
        assert_eq!(word_draft(&name).validate().is_ok(), valid, "{name}");
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
            media_type: DOCX_MEDIA_TYPE.into(),
            size_bytes: 10,
            digest_sha256: "a".repeat(64),
            content: ContentRef::Artifact {
                artifact_id: "created".into(),
                sha256: "a".repeat(64),
                size_bytes: 10,
                media_type: DOCX_MEDIA_TYPE.into(),
            },
        };
        receipt.validate().unwrap();
        receipt.digest_sha256 = "b".repeat(64);
        assert!(receipt.validate().is_err());
    }
}

#[test]
fn existing_adapter_wire_indices_are_unchanged_and_word_is_appended() {
    use ComputerUseAdapterKind::*;
    for (index, kind) in [
        WindowsUia,
        WindowsRawInput,
        MacosBackgroundInput,
        MacosAccessibility,
        OfficeExcel,
        OfficePowerPoint,
        IworkNumbers,
        IworkPages,
        IworkKeynote,
        FileSystem,
        Terminal,
        ScreenCapture,
        SystemDiagnostics,
        BrowserExtension,
        OutlookNewMailto,
        OfficeWord,
    ]
    .into_iter()
    .enumerate()
    {
        let bytes = wincode::serialize(&kind).unwrap();
        assert_eq!(bytes, (index as u32).to_le_bytes());
        assert_eq!(
            wincode::deserialize::<ComputerUseAdapterKind>(&bytes).unwrap(),
            kind
        );
    }
    assert_eq!(
        serde_json::to_string(&OfficeWord).unwrap(),
        "\"office_word\""
    );
}
