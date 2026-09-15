//! Native broker publication; the test supplies an admitted writer lease.
use super::*;
use file_reference_store::windows_publish::PublishFailure;
use sha2::{Digest, Sha256};

#[test]
#[ignore = "requires production LRD_TEST_SERVER_BINARY, synthetic LRD_EXCEL_INPUT and empty LRD_EXCEL_WORK_DIR"]
fn native_excel_copy_requires_lease_preserves_conflicts_and_rejects_cancelled_work() {
    let _guard = file_reference_store::file_store_test_lock();
    let root = std::path::PathBuf::from(std::env::var_os("LRD_EXCEL_WORK_DIR").unwrap());
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    let bytes = std::fs::read(std::env::var_os("LRD_EXCEL_INPUT").unwrap()).unwrap();
    let source_path = root.join("源表格.xlsx");
    std::fs::write(&source_path, &bytes).unwrap();
    let output_dir = root.join("输出目录 with spaces");
    std::fs::create_dir(&output_dir).unwrap();
    let source = file_reference_store::issue(&source_path).unwrap();
    let broker = ComputerUseBroker::new();
    let ceiling = ComputerUseSettings {
        enabled: true,
        observe: true,
        office_semantic: true,
        ..Default::default()
    };
    let readiness = broker.readiness(&ceiling, false, false);
    let observed = broker
        .inspect_excel_cell(&source, "数据", "A1", 4096, &ceiling)
        .unwrap();
    let mut batch = SpreadsheetLiveBatchPatchAction {
        output: desk_agent_protocol::computer_use::BatchDocumentOutput {
            destination_parent: file_reference_store::issue(&output_dir).unwrap(),
            native_file_name: "计算副本.xlsx".into(),
        },
        action: SpreadsheetLivePatchAction::SetCellNumber { value: "42".into() },
    };
    let output_path = output_dir.join(&batch.output.native_file_name);
    assert!(matches!(
        broker.publish_excel_copy_with_lease(
            &observed.range,
            &batch,
            &ceiling,
            "native-generation"
        ),
        Err(PublishFailure::NotCreated(_))
    ));
    assert!(!output_path.exists());
    broker
        .acquire_writer_lease(WriterLeaseRequest {
            scope: WriterLeaseScope::FileWorker,
            work_id: "native-work".into(),
            action_request_id: "native-call".into(),
            execution_generation: "native-generation".into(),
            approved_actor_id: "owner".into(),
            interactive_session_incarnation: readiness.interactive_session_incarnation,
            expires_at: Utc::now() + Duration::seconds(120),
        })
        .unwrap();
    let artifact = broker
        .publish_excel_copy_with_lease(&observed.range, &batch, &ceiling, "native-generation")
        .unwrap();
    let published = std::fs::read(&output_path).unwrap();
    assert_eq!(artifact.sha256, format!("{:x}", Sha256::digest(&published)));
    assert_eq!(std::fs::read(&source_path).unwrap(), bytes);
    let computed = desk_office_batch::xlsx_cells::inspect_stored(&published, "数据", "B1", 4096)
        .unwrap()
        .unwrap();
    assert_eq!(computed.value.as_deref(), Some("84"));
    let Err(PublishFailure::NotCreated(collision)) = broker.publish_excel_copy_with_lease(
        &observed.range,
        &batch,
        &ceiling,
        "native-generation",
    ) else {
        panic!("expected create-new collision")
    };
    assert!(
        collision.message.contains("c0000035"),
        "{}",
        collision.message
    );
    assert_eq!(std::fs::read(&output_path).unwrap(), published);
    broker
        .inspect_excel_cell(&artifact.file, "数据", "B1", 4096, &ceiling)
        .unwrap();
    assert!(broker.cancel_writer_lease(
        &desk_agent_protocol::computer_use::ComputerActionCancel {
            work_id: "native-work".into(),
            action_request_id: "native-call".into(),
            execution_generation: "native-generation".into(),
            reason: "synthetic owner cancellation".into(),
        },
        "owner"
    ));
    batch.output.native_file_name = "已取消.xlsx".into();
    assert!(matches!(
        broker.publish_excel_copy_with_lease(
            &observed.range,
            &batch,
            &ceiling,
            "native-generation"
        ),
        Err(PublishFailure::NotCreated(_))
    ));
    assert!(!output_dir.join(&batch.output.native_file_name).exists());
    assert_eq!(std::fs::read_dir(&output_dir).unwrap().count(), 1);
    assert_eq!(std::fs::read(&source_path).unwrap(), bytes);
    assert!(broker.release_writer_lease("native-generation"));
    println!("published={}", output_path.display());
}
