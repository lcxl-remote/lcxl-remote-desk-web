//! Versioned file-only Office adapter contracts; these never imply readiness.
use super::{ComputerActionKind, ComputerUseAdapterKind, ComputerUseAdapterRef};

pub const PPTX_ADAPTER_VERSION: &str = "office-pptx-batch/v1";
pub const PPTX_MEDIA_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.presentationml.presentation";
const PPTX_VERSION_FAMILY: &str = "office-pptx-batch/";
pub const DOCX_ADAPTER_VERSION: &str = "office-docx-batch/v1";
pub const DOCX_MEDIA_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
pub const XLSX_ADAPTER_VERSION: &str = "office-xlsx-batch/v1";
pub const XLSX_MEDIA_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

pub fn is_xlsx(adapter: &ComputerUseAdapterRef) -> bool {
    adapter.kind == ComputerUseAdapterKind::OfficeExcel && adapter.version == XLSX_ADAPTER_VERSION
}

pub fn is_docx(adapter: &ComputerUseAdapterRef) -> bool {
    adapter.kind == ComputerUseAdapterKind::OfficeWord && adapter.version == DOCX_ADAPTER_VERSION
}

pub fn is_pptx(adapter: &ComputerUseAdapterRef) -> bool {
    adapter.kind == ComputerUseAdapterKind::OfficePowerPoint
        && adapter.version == PPTX_ADAPTER_VERSION
}

pub(super) fn action_support(
    adapter: &ComputerUseAdapterRef,
    action: &ComputerActionKind,
) -> Option<bool> {
    use super::SpreadsheetLivePatchAction as A;
    let explicit_scalar = match action {
        ComputerActionKind::SpreadsheetLive(a) => {
            matches!(a, A::SetCellNumber { .. } | A::SetCellBoolean { .. })
        }
        ComputerActionKind::SpreadsheetLiveBatch(a) => matches!(
            &a.action,
            A::SetCellNumber { .. } | A::SetCellBoolean { .. }
        ),
        _ => false,
    };
    if explicit_scalar && !is_xlsx(adapter) {
        return Some(false);
    }
    if adapter.version.starts_with("office-xlsx-batch/") {
        return Some(
            is_xlsx(adapter) && matches!(action, ComputerActionKind::SpreadsheetLiveBatch(_)),
        );
    }
    if adapter.version.starts_with("office-docx-batch/") {
        return Some(
            is_docx(adapter) && matches!(action, ComputerActionKind::DocumentLiveBatch(_)),
        );
    }
    adapter
        .version
        .starts_with(PPTX_VERSION_FAMILY)
        .then(|| is_pptx(adapter) && matches!(action, ComputerActionKind::PresentationLiveBatch(_)))
}

pub fn valid_pptx_leaf(name: &str) -> bool {
    valid_office_leaf(name, ".pptx")
}

pub fn valid_docx_leaf(name: &str) -> bool {
    valid_office_leaf(name, ".docx")
}

pub fn valid_xlsx_leaf(name: &str) -> bool {
    valid_office_leaf(name, ".xlsx")
}

pub fn valid_number_literal(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.trim() == value
        && value.parse::<f64>().ok().is_some_and(f64::is_finite)
}

fn valid_office_leaf(name: &str, extension: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if !lower.ends_with(extension)
        || name.len() <= extension.len()
        || name.len() > 255
        || name
            .chars()
            .any(|c| c.is_control() || "\\/:*?\"<>|".contains(c))
        || name.ends_with(['.', ' '])
    {
        return false;
    }
    let base = lower
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ');
    !matches!(base, "con" | "prn" | "aux" | "nul" | "conin$" | "conout$")
        && !["com", "lpt"].iter().any(|prefix| {
            base.strip_prefix(prefix).is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        })
}

#[cfg(test)]
mod tests {
    mod excel;
    mod word;
    use super::super::*;
    use super::*;
    fn draft() -> ComputerActionDraft {
        let object = |kind, token: &str| ObjectRef {
            object_kind: kind,
            token: token.into(),
            snapshot_id: "worker:1".into(),
            expires_at: "2030-01-01T00:00:00Z".into(),
        };
        ComputerActionDraft {
            schema_version: COMPUTER_USE_SCHEMA_VERSION,
            adapter: ComputerUseAdapterRef {
                kind: ComputerUseAdapterKind::OfficePowerPoint,
                version: PPTX_ADAPTER_VERSION.into(),
            },
            risk: crate::RiskLevel::Medium,
            reversible: true,
            data_egress: false,
            actions: vec![ComputerActionStep {
                target: object(ObjectKind::Slide, "slide"),
                action: ComputerActionKind::PresentationLiveBatch(
                    PresentationLiveBatchPatchAction {
                        output: BatchDocumentOutput {
                            destination_parent: object(ObjectKind::Directory, "output"),
                            native_file_name: "reviewed.PPTX".into(),
                        },
                        action: PresentationLivePatchAction::SetPresenterNotes {
                            text: "notes".into(),
                        },
                    },
                ),
                before_summary: "original notes".into(),
                after_intent: "new copy".into(),
                verification: "read back".into(),
            }],
        }
    }
    #[test]
    fn pptx_version_accepts_only_file_batch_actions() {
        let good = draft();
        good.validate().unwrap();
        for (kind, version) in [
            (
                ComputerUseAdapterKind::OfficePowerPoint,
                "office-pptx-batch/v2",
            ),
            (ComputerUseAdapterKind::IworkKeynote, PPTX_ADAPTER_VERSION),
            (
                ComputerUseAdapterKind::OfficePowerPoint,
                "office-js-bridge-read/v1",
            ),
        ] {
            let mut changed = good.clone();
            changed.adapter.kind = kind;
            changed.adapter.version = version.into();
            assert_eq!(
                changed.validate(),
                Err(ComputerUseValidationError::IncompatibleActionAdapter)
            );
        }
        for action in [
            ComputerActionKind::PresentationLive(PresentationLivePatchAction::SetPresenterNotes {
                text: "live".into(),
            }),
            ComputerActionKind::PowerPoint(PowerPointPatchAction::DeleteShape),
        ] {
            let mut changed = good.clone();
            changed.actions[0].action = action;
            assert_eq!(
                changed.validate(),
                Err(ComputerUseValidationError::IncompatibleActionAdapter)
            );
        }
    }
    #[test]
    fn pptx_receipt_uses_the_same_leaf_limit_as_the_reviewed_action() {
        for (name, accepted) in [
            (format!("{}.pptx", "a".repeat(250)), true),
            (format!("{}.PPTX", "中".repeat(83)), true),
            (format!("{}.pptx", "a".repeat(251)), false),
            (format!("{}.pptx", "中".repeat(84)), false),
            ("CON.pptx".into(), false),
        ] {
            let mut request = draft();
            let ComputerActionKind::PresentationLiveBatch(batch) = &mut request.actions[0].action
            else {
                unreachable!()
            };
            batch.output.native_file_name = name.clone();
            let mut artifact = CreatedFileArtifactOutput {
                file: ObjectRef {
                    object_kind: ObjectKind::File,
                    token: "created".into(),
                    snapshot_id: "worker:1".into(),
                    expires_at: "2030-01-01T00:00:00Z".into(),
                },
                file_name: name.clone(),
                media_type: PPTX_MEDIA_TYPE.into(),
                size_bytes: 1,
                digest_sha256: "a".repeat(64),
                content: crate::data_lineage::ContentRef::Artifact {
                    artifact_id: "created".into(),
                    sha256: "a".repeat(64),
                    size_bytes: 1,
                    media_type: PPTX_MEDIA_TYPE.into(),
                },
            };
            assert_eq!(request.validate().is_ok(), accepted, "action: {name}");
            assert_eq!(artifact.validate().is_ok(), accepted, "receipt: {name}");
            if accepted {
                artifact.digest_sha256 = "b".repeat(64);
                assert!(
                    artifact.validate().is_err(),
                    "digest binding remains required"
                );
            }
        }
    }

    #[test]
    fn pptx_output_rejects_paths_device_names_and_streams() {
        for name in [
            "../a.pptx",
            "a:b.pptx",
            "CON.pptx",
            "com1.pptx",
            "LPT².pptx",
            "a?.pptx",
            "a\0.pptx",
            "wrong.key",
            ".pptx",
        ] {
            let mut changed = draft();
            let ComputerActionKind::PresentationLiveBatch(action) = &mut changed.actions[0].action
            else {
                unreachable!()
            };
            action.output.native_file_name = name.into();
            assert!(changed.validate().is_err(), "{name}");
        }
    }
}
