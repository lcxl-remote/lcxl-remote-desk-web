use super::*;
use crate::host_control::{ApprovalResponse, HostControlMessage};

fn text(connection: &str, message: FileTransferMessage) -> FileTransferPayload {
    FileTransferPayload {
        connection_id: connection.into(),
        data: serde_json::to_vec(&message).unwrap(),
        is_text: true,
        transfer_id: None,
    }
}

fn request(
    id: &str,
    name: &str,
    directory: Option<&std::path::Path>,
    bytes: u64,
) -> FileTransferMessage {
    FileTransferMessage::UploadRequest(UploadRequest {
        transfer_id: id.into(),
        target_dir: directory.map(|dir| dir.to_string_lossy().into_owned()),
        target_location: directory.is_none().then_some(UploadLocation::Downloads),
        file_name: name.into(),
        file_size: bytes,
        chunk_size: 4,
        total_chunks: u64::from(bytes != 0),
    })
}

async fn accepted(rx: &mut Box<dyn EventReceiver<FileTransferPayload>>) -> UploadResponse {
    loop {
        match expect_message(rx).await {
            FileTransferMessage::UploadResponse(response) => return response,
            FileTransferMessage::UploadStateChanged(_) => {}
            other => panic!("expected upload acceptance, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn downloads_preserve_existing_names_and_resolve_an_external_race() {
    let directory = TempDir::new().unwrap();
    let (mut d, mut rx) = dispatcher();
    d.download_directory = Some(directory.path().into());
    d.start_connection(&start_payload("c1")).await;
    tokio::fs::write(directory.path().join("report.txt"), b"original")
        .await
        .unwrap();
    for (id, expected) in [
        ("00000000-0000-4000-8000-000000000001", "report (2).txt"),
        ("00000000-0000-4000-8000-000000000002", "report (3).txt"),
    ] {
        d.handle_command(text("c1", request(id, "report.txt", None, 3)))
            .await;
        let response = accepted(&mut rx).await;
        if id.ends_with('1') {
            assert_eq!(response.resolved_file_name, "report (1).txt");
            tokio::fs::write(
                directory.path().join(&response.resolved_file_name),
                b"external",
            )
            .await
            .unwrap();
        }
        d.handle_command(FileTransferPayload {
            connection_id: "c1".into(),
            data: build_binary_chunk(id, 0, b"new"),
            is_text: false,
            transfer_id: None,
        })
        .await;
        match expect_message(&mut rx).await {
            FileTransferMessage::TransferComplete(result) => {
                assert_eq!(result.resolved_file_name.as_deref(), Some(expected))
            }
            other => panic!("expected completion, got {other:?}"),
        }
        assert_eq!(
            tokio::fs::read(directory.path().join(expected))
                .await
                .unwrap(),
            b"new"
        );
    }
    assert_eq!(
        tokio::fs::read(directory.path().join("report.txt"))
            .await
            .unwrap(),
        b"original"
    );
    assert_eq!(
        tokio::fs::read(directory.path().join("report (1).txt"))
            .await
            .unwrap(),
        b"external"
    );
    assert!(d.inner.lock().await.upload_destinations.is_empty());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 4);
}

#[test]
fn completion_fields_follow_message_role() {
    for (id, name) in [
        ("host-upload", Some("report (1).txt")),
        ("host-download", None),
        ("client-upload-end", None),
    ] {
        let message = FileTransferMessage::TransferComplete(TransferComplete {
            transfer_id: id.into(),
            resolved_file_name: name.map(str::to_owned),
            resolved_target_dir: None,
        });
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value.get("resolved_file_name").is_some(), name.is_some());
        assert!(value.get("resolved_target_dir").is_none());
        let FileTransferMessage::TransferComplete(decoded) = serde_json::from_value(value).unwrap()
        else {
            panic!("wrong message")
        };
        assert_eq!(decoded.resolved_file_name.as_deref(), name);
    }
}

#[tokio::test]
async fn pending_approval_shares_one_question_and_does_not_block_another_connection() {
    let directory = TempDir::new().unwrap();
    let (mut d, mut replies) = dispatcher();
    d.policy = PolicyAccess::for_test(SecuritySettings {
        allow_file_transfer: Some(true),
        approval_timeout: Some(0),
        ..SecuritySettings::default()
    })
    .0;
    let mut ui = d.hub.subscribe_outbound();
    d.hub.mark_tauri_connected();
    d.start_connection(&start_payload("a")).await;
    d.start_connection(&start_payload("b")).await;
    d.connection_ceilings
        .set(
            "b",
            Some(SecuritySettings {
                allow_file_transfer: None,
                ..SecuritySettings::default()
            }),
        )
        .await;
    let (lane, mut inbound) = inprocess::make_event_inprocess_with_cap::<FileTransferPayload>(16);
    let owner = d.clone();
    let drain = tokio::spawn(async move {
        while let Some(payload) = inbound.recv().await {
            owner.handle_command(payload).await;
        }
    });
    let a = "00000000-0000-4000-8000-000000000003";
    lane.send(text("a", request(a, "a.txt", Some(directory.path()), 3)))
        .await
        .unwrap();
    accepted(&mut replies).await;
    lane.send(text(
        "b",
        request("b1", "b1.txt", Some(directory.path()), 0),
    ))
    .await
    .unwrap();
    let prompt_id = loop {
        if let HostControlMessage::SecurityApprovalRequest { req_id, .. } =
            tokio::time::timeout(Duration::from_secs(2), ui.recv())
                .await
                .unwrap()
                .unwrap()
        {
            break req_id;
        }
    };
    lane.send(text(
        "b",
        request("b2", "b2.txt", Some(directory.path()), 0),
    ))
    .await
    .unwrap();
    assert!(d.hub.notify_approval_ack(&prompt_id));
    tokio::task::yield_now().await;
    // The host's unlimited approval remains pending beyond the client's old total timeout.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(40)).await;
    tokio::time::resume();
    assert_eq!(d.hub.pending_replay_count(), 1);
    let download_key = TransferKey::new("a", "download-to-cancel");
    d.inner
        .lock()
        .await
        .live_downloads
        .insert(download_key.clone());
    lane.send(text(
        "a",
        FileTransferMessage::TransferCancel(TransferCancel {
            transfer_id: "download-to-cancel".into(),
        }),
    ))
    .await
    .unwrap();
    lane.send(FileTransferPayload {
        connection_id: "a".into(),
        data: build_binary_chunk(a, 0, b"abc"),
        is_text: false,
        transfer_id: None,
    })
    .await
    .unwrap();
    loop {
        if let FileTransferMessage::TransferComplete(complete) = expect_message(&mut replies).await
        {
            assert_eq!(complete.transfer_id, a);
            break;
        }
    }
    assert_eq!(
        tokio::fs::read(directory.path().join("a.txt"))
            .await
            .unwrap(),
        b"abc"
    );
    assert_eq!(d.hub.pending_replay_count(), 1);
    assert!(
        d.inner
            .lock()
            .await
            .cancelled_transfers
            .contains(&download_key)
    );
    assert!(!directory.path().join("b1.txt").exists());
    lane.send(text(
        "b",
        FileTransferMessage::TransferCancel(TransferCancel {
            transfer_id: "b1".into(),
        }),
    ))
    .await
    .unwrap();
    while d
        .inner
        .lock()
        .await
        .pending_requests
        .contains_key(&TransferKey::new("b", "b1"))
    {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        d.hub.pending_replay_count(),
        1,
        "one cancelled consumer must not close the shared question"
    );
    assert!(d.hub.submit_approval(
        &prompt_id,
        ApprovalResponse {
            approved: true,
            remember: false
        }
    ));
    let response = accepted(&mut replies).await;
    assert_eq!(response.transfer_id, "b2");
    assert!(!directory.path().join("b1.txt").exists());
    d.shutdown().await;
    drain.abort();
}

#[tokio::test]
async fn cancelling_last_pending_consumer_closes_only_its_question_and_late_approval_is_ignored() {
    let directory = TempDir::new().unwrap();
    let (d, _replies) = dispatcher_with_setting(None);
    let mut ui = d.hub.subscribe_outbound();
    d.hub.mark_tauri_connected();
    d.start_connection(&start_payload("c1")).await;
    d.handle_command(text(
        "c1",
        request("cancel-me", "never.txt", Some(directory.path()), 0),
    ))
    .await;
    let prompt = loop {
        if let HostControlMessage::SecurityApprovalRequest { req_id, .. } = ui.recv().await.unwrap()
        {
            break req_id;
        }
    };
    d.handle_command(text(
        "c1",
        FileTransferMessage::TransferCancel(TransferCancel {
            transfer_id: "cancel-me".into(),
        }),
    ))
    .await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while d.hub.pending_replay_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!d.hub.submit_approval(
        &prompt,
        ApprovalResponse {
            approved: true,
            remember: false
        }
    ));
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    assert!(d.inner.lock().await.upload_destinations.is_empty());
}

struct SyncGate {
    file: tokio::fs::File,
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: Option<tokio::sync::oneshot::Receiver<()>>,
}

#[async_trait::async_trait]
impl UploadSink for SyncGate {
    async fn sync(&mut self) -> std::io::Result<()> {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        if let Some(release) = self.release.take() {
            let _ = release.await;
        }
        self.file.sync_all().await
    }
}

impl tokio::io::AsyncWrite for SyncGate {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.file).poll_write(context, bytes)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.file).poll_flush(context)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.file).poll_shutdown(context)
    }
}

#[tokio::test]
async fn both_completion_paths_hold_the_destination_through_file_sync() {
    for bytes in [0, 3] {
        let directory = TempDir::new().unwrap();
        let (d, mut rx) = dispatcher();
        d.start_connection(&start_payload("c1")).await;
        let parent = directory.path().canonicalize().unwrap();
        let destination = parent.join("saved.txt");
        let staging = parent.join("stage.part");
        let file = tokio::fs::File::create(&staging).await.unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let id = "00000000-0000-4000-8000-000000000004";
        arm_upload(
            &d,
            "c1",
            id,
            destination.clone(),
            staging,
            Box::new(SyncGate {
                file,
                entered: Some(entered_tx),
                release: Some(release_rx),
            }),
            u64::from(bytes != 0),
            bytes,
        )
        .await;
        let command = if bytes == 0 {
            text(
                "c1",
                FileTransferMessage::TransferComplete(TransferComplete {
                    transfer_id: id.into(),
                    resolved_file_name: None,
                    resolved_target_dir: None,
                }),
            )
        } else {
            FileTransferPayload {
                connection_id: "c1".into(),
                data: build_binary_chunk(id, 0, b"abc"),
                is_text: false,
                transfer_id: None,
            }
        };
        let owner = d.clone();
        let commit = tokio::spawn(async move {
            owner.handle_command(command).await;
        });
        entered_rx.await.unwrap();
        assert!(
            d.inner
                .lock()
                .await
                .upload_destinations
                .contains(&destination)
        );
        assert!(!destination.exists(), "no publication before file sync");
        d.handle_command(text(
            "c1",
            request("another", "saved.txt", Some(&parent), 0),
        ))
        .await;
        assert!(
            matches!(expect_message(&mut rx).await, FileTransferMessage::TransferError(error) if error.error_code == DeskErrorCode::INVALID_STATE)
        );
        assert!(
            d.inner
                .lock()
                .await
                .upload_destinations
                .contains(&destination)
        );
        release_tx.send(()).unwrap();
        commit.await.unwrap();
        assert!(matches!(
            expect_message(&mut rx).await,
            FileTransferMessage::TransferComplete(_)
        ));
        assert_eq!(
            tokio::fs::read(destination).await.unwrap(),
            if bytes == 0 {
                b"".as_slice()
            } else {
                b"abc".as_slice()
            }
        );
        assert!(d.inner.lock().await.upload_destinations.is_empty());
    }
}

struct ClosedSender;

#[async_trait::async_trait]
impl EventSender<FileTransferPayload> for ClosedSender {
    async fn send(&self, _: FileTransferPayload) -> Result<(), TransportError> {
        Err(TransportError::Closed)
    }
}

#[tokio::test]
async fn a_failed_acceptance_reply_closes_and_removes_the_staged_upload() {
    let directory = TempDir::new().unwrap();
    let (mut d, _rx) = dispatcher();
    d.start_connection(&start_payload("c1")).await;
    d.file_sender = Arc::new(ClosedSender);
    let FileTransferMessage::UploadRequest(req) =
        request("not-accepted", "partial.txt", Some(directory.path()), 3)
    else {
        unreachable!()
    };
    d.accept_upload("c1".into(), req, None).await.unwrap();
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    assert_eq!(d.active_transfer_count().await, 0);
    let inner = d.inner.lock().await;
    assert!(inner.upload_states.is_empty());
    assert!(inner.upload_destinations.is_empty());
}

#[tokio::test]
async fn a_lost_completion_reply_does_not_undo_publication_or_report_it_as_failed() {
    for bytes in [0, 3] {
        let directory = TempDir::new().unwrap();
        let (mut d, mut rx) = dispatcher();
        let (activity_tx, mut activity_rx) = mpsc::unbounded_channel();
        d.activity_sender = activity_tx;
        d.start_connection(&start_payload("c1")).await;
        let id = "00000000-0000-4000-8000-000000000005";
        d.handle_command(text(
            "c1",
            request(id, "saved.txt", Some(directory.path()), bytes),
        ))
        .await;
        accepted(&mut rx).await;
        d.file_sender = Arc::new(ClosedSender);
        let command = if bytes == 0 {
            text(
                "c1",
                FileTransferMessage::TransferComplete(TransferComplete {
                    transfer_id: id.into(),
                    resolved_file_name: None,
                    resolved_target_dir: None,
                }),
            )
        } else {
            FileTransferPayload {
                connection_id: "c1".into(),
                data: build_binary_chunk(id, 0, b"abc"),
                is_text: false,
                transfer_id: None,
            }
        };
        d.handle_command(command).await;
        assert_eq!(
            tokio::fs::read(directory.path().join("saved.txt"))
                .await
                .unwrap()
                .len(),
            bytes as usize
        );
        assert!(matches!(
            activity_rx.recv().await.unwrap(),
            WorkerToService::FileTransferStarted(_)
        ));
        assert!(
            matches!(activity_rx.recv().await.unwrap(), WorkerToService::FileTransferFinished(event) if event.outcome == FileTransferOutcome::Completed)
        );
        assert_eq!(d.active_transfer_count().await, 0);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}

#[tokio::test]
async fn a_revoked_upload_releases_staging_even_when_the_client_has_sent_its_last_chunk() {
    let directory = TempDir::new().unwrap();
    let (mut d, mut rx) = dispatcher();
    let (policy, mirror, _upstream) = PolicyAccess::for_test(SecuritySettings {
        allow_file_transfer: Some(true),
        ..SecuritySettings::default()
    });
    d.policy = policy;
    d.start_connection(&start_payload("c1")).await;
    let id = "00000000-0000-4000-8000-000000000006";
    tokio::fs::write(directory.path().join("keep.txt"), b"original")
        .await
        .unwrap();
    let mut upload = request(id, "keep.txt", Some(directory.path()), 6);
    if let FileTransferMessage::UploadRequest(req) = &mut upload {
        req.total_chunks = 2;
        req.chunk_size = 3;
    }
    d.handle_command(text("c1", upload)).await;
    accepted(&mut rx).await;
    let chunk = |index| FileTransferPayload {
        connection_id: "c1".into(),
        data: build_binary_chunk(id, index, b"abc"),
        is_text: false,
        transfer_id: None,
    };
    d.handle_command(chunk(0)).await;
    let mut published = mirror.snapshot();
    published.set_capability(SecurityPermissionType::FileTransfer, Some(false));
    mirror.apply(published);
    d.handle_command(chunk(1)).await;
    assert!(
        matches!(expect_message(&mut rx).await, FileTransferMessage::TransferError(error) if error.error_code == DeskErrorCode::PERMISSION_ERROR)
    );
    assert_eq!(
        tokio::fs::read(directory.path().join("keep.txt"))
            .await
            .unwrap(),
        b"original"
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    assert_eq!(d.active_transfer_count().await, 0);
    assert!(d.inner.lock().await.upload_destinations.is_empty());
}
