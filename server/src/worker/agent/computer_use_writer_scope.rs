//! Dispatcher-owned classification. A remote plan cannot choose its lease scope.
use super::WriterLeaseScope;
use desk_agent_protocol::computer_use::{ComputerActionKind, ComputerUseAdapterRef};

pub(crate) fn scope_for_action(
    action: &ComputerActionKind,
    adapter: &ComputerUseAdapterRef,
) -> WriterLeaseScope {
    match action {
        ComputerActionKind::BackgroundInput { .. } => WriterLeaseScope::BackgroundApplication,
        ComputerActionKind::File(_) => WriterLeaseScope::FileWorker,
        _ if is_file_batch(action, adapter) => WriterLeaseScope::FileWorker,
        _ => WriterLeaseScope::InteractiveSession,
    }
}

#[cfg(windows)]
fn is_file_batch(action: &ComputerActionKind, adapter: &ComputerUseAdapterRef) -> bool {
    use desk_agent_protocol::computer_use::office_batch;
    match action {
        ComputerActionKind::SpreadsheetLiveBatch(_) => office_batch::is_xlsx(adapter),
        ComputerActionKind::DocumentLiveBatch(_) => office_batch::is_docx(adapter),
        ComputerActionKind::PresentationLiveBatch(_) => office_batch::is_pptx(adapter),
        _ => false,
    }
}

#[cfg(not(windows))]
fn is_file_batch(_: &ComputerActionKind, _: &ComputerUseAdapterRef) -> bool {
    // macOS iWork batch actions still automate an interactive application.
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::computer_use::*;

    #[test]
    fn only_matching_windows_file_batch_adapters_use_file_worker() {
        let output = BatchDocumentOutput {
            destination_parent: ObjectRef {
                token: "directory".into(),
                snapshot_id: "snapshot".into(),
                object_kind: ObjectKind::Directory,
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
            native_file_name: "copy".into(),
        };
        let cases = [
            (
                ComputerActionKind::SpreadsheetLiveBatch(SpreadsheetLiveBatchPatchAction {
                    output: output.clone(),
                    action: SpreadsheetLivePatchAction::SetCellNumber { value: "1".into() },
                }),
                ComputerUseAdapterKind::OfficeExcel,
                office_batch::XLSX_ADAPTER_VERSION,
            ),
            (
                ComputerActionKind::DocumentLiveBatch(DocumentLiveBatchPatchAction {
                    output: output.clone(),
                    action: DocumentLivePatchAction::ReplaceBodyText {
                        text: "copy".into(),
                    },
                }),
                ComputerUseAdapterKind::OfficeWord,
                office_batch::DOCX_ADAPTER_VERSION,
            ),
            (
                ComputerActionKind::PresentationLiveBatch(PresentationLiveBatchPatchAction {
                    output,
                    action: PresentationLivePatchAction::SetPresenterNotes {
                        text: "copy".into(),
                    },
                }),
                ComputerUseAdapterKind::OfficePowerPoint,
                office_batch::PPTX_ADAPTER_VERSION,
            ),
        ];
        for (action, kind, version) in cases {
            let adapter = ComputerUseAdapterRef {
                kind,
                version: version.into(),
            };
            assert_eq!(
                scope_for_action(&action, &adapter),
                if cfg!(windows) {
                    WriterLeaseScope::FileWorker
                } else {
                    WriterLeaseScope::InteractiveSession
                }
            );
            for other in [
                ComputerUseAdapterRef {
                    kind: adapter.kind.clone(),
                    version: "live/v1".into(),
                },
                ComputerUseAdapterRef {
                    kind: ComputerUseAdapterKind::FileSystem,
                    version: version.into(),
                },
            ] {
                assert_eq!(
                    scope_for_action(&action, &other),
                    WriterLeaseScope::InteractiveSession
                );
            }
            let live = ComputerActionKind::DocumentLive(DocumentLivePatchAction::ReplaceBodyText {
                text: "live".into(),
            });
            assert_eq!(
                scope_for_action(&live, &adapter),
                WriterLeaseScope::InteractiveSession
            );
        }
    }
}
