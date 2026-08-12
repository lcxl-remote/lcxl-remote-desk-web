use super::*;

/// The owner-plane host-management frames the central refuses to forward
/// from a capability-scoped code-session (`device_user`). Session-scoped media
/// tuning (`UpdateDeskSettings`) and the session/capability frames must NOT be
/// classified owner-plane, or a legitimate support session would break.
#[test]
fn owner_plane_frames_are_classified_for_code_session_denial() {
    use SignalingType::*;
    for t in [GetSystemInfo, ChangeDisplaySettings] {
        assert!(
            is_owner_plane_management_frame(t),
            "{t:?} must be owner-plane (denied for a code-session)"
        );
    }
    for t in [
        RequestRemoteAccess,
        Offer,
        Answer,
        RequireControl,
        ReleaseControl,
        CloseRemoteSession,
        StartTerminal,
        ListTerminalCommands,
        ListFiles,
        DeleteFile,
        SetPrivateScreenVisibility,
        UpdateDeskSettings,
        RetryMediaPipeline,
    ] {
        assert!(
            !is_owner_plane_management_frame(t),
            "{t:?} must NOT be owner-plane (a code-session legitimately uses it)"
        );
    }
}

#[test]
fn media_pipeline_signaling_discriminants_are_stable() {
    assert_eq!(SignalingType::MediaPipelineStateChanged as i32, 217);
    assert_eq!(SignalingType::RetryMediaPipeline as i32, 218);
    assert_eq!(
        serde_json::to_string(&SignalingType::MediaPipelineStateChanged).unwrap(),
        "217"
    );
    assert!(matches!(
        serde_json::from_str::<SignalingType>("218").unwrap(),
        SignalingType::RetryMediaPipeline
    ));
}

/// `ConnectionRemoved` is the wire-level marker the daemon's
/// signaling router keys off to release per-`connection_id`
/// resources. The integer discriminant must stay stable across
/// releases — bumping it would silently desync browsers /
/// daemons running mismatched builds, and the active cleanup
/// path the daemon depends on would just drop on the floor at
/// `SignalingType::Unknown`. Pin both the discriminant and the
/// JSON wire form.
#[test]
fn signaling_type_connection_removed_wire_format_is_stable() {
    // Discriminant: integer 23 is what the JSON deserializer reads.
    // Hard-coded both sides so a `repr(i32)` reorder breaks the test
    // instead of silently shifting the enum's wire value.
    assert_eq!(SignalingType::ConnectionRemoved as i32, 23);

    let json = serde_json::to_string(&SignalingType::ConnectionRemoved)
        .expect("serialize ConnectionRemoved");
    assert_eq!(json, "23");

    let parsed: SignalingType =
        serde_json::from_str("23").expect("deserialize 23 -> ConnectionRemoved");
    assert!(matches!(parsed, SignalingType::ConnectionRemoved));
}

/// The terminal command-completion discriminants must stay stable: a browser
/// and a manager / host on mismatched builds would otherwise desync, with the
/// frame silently collapsing to `SignalingType::Unknown`. The ask (620) routes
/// through the AI authorizer branch and the result (621) through the plain
/// host → control relay branch, exactly like the copilot ask/event pair.
#[test]
fn terminal_complete_signaling_discriminants_are_stable() {
    assert_eq!(SignalingType::GenerateTerminalCompletions as i32, 620);
    assert_eq!(SignalingType::TerminalCompletionsGenerated as i32, 621);
    assert_eq!(
        serde_json::to_string(&SignalingType::GenerateTerminalCompletions).unwrap(),
        "620"
    );
    let parsed: SignalingType = serde_json::from_str("621").unwrap();
    assert!(matches!(
        parsed,
        SignalingType::TerminalCompletionsGenerated
    ));
}

/// Empty map (no `Server`-type peers around) must skip the
/// broadcast cleanly. This covers the early-exit path that keeps
/// the helper safe to call from a `Drop` background task — even
/// when the connection map has already been drained.
#[tokio::test]
async fn broadcast_connection_removed_to_servers_no_op_on_empty_map() {
    let empty = SharedConnectionMap::new();
    // Should return without blocking on anything; the assertion is
    // simply that the future completes promptly under the test
    // runtime's default no-IO budget.
    broadcast_connection_removed_to_servers("conn-bye", &empty).await;
    assert_eq!(empty.read().await.len(), 0);
}

/// A target connection absent from this instance's map yields `Ok(false)`
/// (a local miss), the signal `forward_to_peer` uses to fall back to the
/// cross-instance relay.
#[tokio::test]
async fn deliver_to_local_peer_reports_miss_when_absent() {
    let empty = SharedConnectionMap::new();
    let model = SignalingModel::new(
        "req-1",
        SignalingType::Offer,
        Some("from".to_string()),
        Some("missing".to_string()),
        None,
        None,
    );
    let delivered = deliver_to_local_peer(&empty, "from", "missing", &model)
        .await
        .expect("miss path is not an error");
    assert!(!delivered);
}

/// Records the outcome a mock relay should return, so the seam decision can be
/// exercised without a live WS `Session`.
struct MockRelay {
    outcome: RelayOutcome,
    called: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl PeerFrameRelay for MockRelay {
    fn relay<'a>(
        &'a self,
        _to: &'a str,
        _from: &'a str,
        _model: &'a SignalingModel,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<RelayOutcome, DeskSignalFacadeError>> + 'a>,
    > {
        self.called.store(true, std::sync::atomic::Ordering::SeqCst);
        let outcome = self.outcome;
        Box::pin(async move { Ok(outcome) })
    }
}

fn dummy_model() -> SignalingModel {
    SignalingModel::new(
        "req-1",
        SignalingType::Answer,
        Some("from".to_string()),
        Some("to".to_string()),
        None,
        None,
    )
}

fn is_session_not_found(err: &DeskSignalFacadeError) -> bool {
    matches!(
        err,
        DeskSignalFacadeError::CustomError(e) if e.error_code == DeskErrorCode::SESSION_NOT_FOUND
    )
}

/// No relay (the signal server) + a local miss is a genuine SESSION_NOT_FOUND.
#[tokio::test]
async fn relay_or_not_found_without_relay_is_session_not_found() {
    let model = dummy_model();
    let err = relay_or_not_found(&None, "to", "from", &model, false)
        .await
        .expect_err("a local miss with no relay must error");
    assert!(is_session_not_found(&err));
}

/// No relay + `ignore_connection_not_found` swallows the miss (best-effort
/// frames like terminal replies).
#[tokio::test]
async fn relay_or_not_found_without_relay_honors_ignore_flag() {
    let model = dummy_model();
    relay_or_not_found(&None, "to", "from", &model, true)
        .await
        .expect("ignore flag turns a miss into Ok");
}

/// A relay that delivers the frame resolves the forward successfully.
#[tokio::test]
async fn relay_or_not_found_delivered_is_ok() {
    let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let relay: Option<Arc<dyn PeerFrameRelay>> = Some(Arc::new(MockRelay {
        outcome: RelayOutcome::Delivered,
        called: called.clone(),
    }));
    let model = dummy_model();
    relay_or_not_found(&relay, "to", "from", &model, false)
        .await
        .expect("a delivered relay resolves Ok");
    assert!(called.load(std::sync::atomic::Ordering::SeqCst));
}

/// A relay that reports the target held by no instance still surfaces
/// SESSION_NOT_FOUND (when not ignoring), and honors the ignore flag otherwise.
#[tokio::test]
async fn relay_or_not_found_not_found_falls_through() {
    let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let relay: Option<Arc<dyn PeerFrameRelay>> = Some(Arc::new(MockRelay {
        outcome: RelayOutcome::NotFound,
        called: called.clone(),
    }));
    let model = dummy_model();
    let err = relay_or_not_found(&relay, "to", "from", &model, false)
        .await
        .expect_err("relay NotFound with no ignore must error");
    assert!(is_session_not_found(&err));
    assert!(called.load(std::sync::atomic::Ordering::SeqCst));

    relay_or_not_found(&relay, "to", "from", &model, true)
        .await
        .expect("relay NotFound with ignore is Ok");
}

/// Build a frame with the given target and response/request nature.
fn frame(to: Option<&str>, is_response: bool) -> SignalingModel {
    SignalingModel::new(
        "req-1",
        SignalingType::ListTerminalCommands,
        None,
        to.map(str::to_string),
        None,
        is_response.then(crate::model::signal::SignalingResponseState::success),
    )
}

/// A body-less response with no `to_connection_id` (e.g. a `ListTerminalCommands`
/// response the daemon broadcast to a non-owning upstream) is dropped, not
/// surfaced as "To connection id can't be none".
#[test]
fn classify_orphaned_response_without_target_is_dropped() {
    let model = frame(None, true);
    assert!(matches!(
        classify_unmatched_forward(&model, false),
        UnmatchedForward::Drop
    ));
}

/// A *request* with no target is still a protocol error (a client must address
/// its request), regardless of the ignore flag.
#[test]
fn classify_request_without_target_is_missing_target() {
    let model = frame(None, false);
    assert!(matches!(
        classify_unmatched_forward(&model, true),
        UnmatchedForward::MissingTarget
    ));
}

/// A response addressed to a connection this central does not hold (e.g. a
/// `TerminalStarted` broadcast to a non-owning upstream) is delivered
/// best-effort: a local + relay miss must be ignored, never SESSION_NOT_FOUND.
#[test]
fn classify_response_with_target_ignores_miss() {
    let model = frame(Some("peer"), true);
    match classify_unmatched_forward(&model, false) {
        UnmatchedForward::Deliver {
            to,
            ignore_not_found,
        } => {
            assert_eq!(to, "peer");
            assert!(ignore_not_found, "a response miss must be benign");
        }
        other => panic!(
            "expected Deliver, got a different variant: {}",
            match other {
                UnmatchedForward::Drop => "Drop",
                UnmatchedForward::MissingTarget => "MissingTarget",
                UnmatchedForward::Deliver { .. } => unreachable!(),
            }
        ),
    }
}

/// A *request* addressed to an absent connection keeps strict semantics: with
/// no ignore flag a miss surfaces SESSION_NOT_FOUND so a control end learns the
/// peer is offline.
#[test]
fn classify_request_with_target_is_strict_by_default() {
    let model = frame(Some("peer"), false);
    match classify_unmatched_forward(&model, false) {
        UnmatchedForward::Deliver {
            to,
            ignore_not_found,
        } => {
            assert_eq!(to, "peer");
            assert!(!ignore_not_found, "a request miss must stay strict");
        }
        _ => panic!("expected Deliver"),
    }
}

/// The explicit ignore flag still wins for a request (best-effort terminal
/// replies pass it), so a miss is swallowed.
#[test]
fn classify_request_with_target_honors_explicit_ignore() {
    let model = frame(Some("peer"), false);
    match classify_unmatched_forward(&model, true) {
        UnmatchedForward::Deliver {
            ignore_not_found, ..
        } => assert!(ignore_not_found),
        _ => panic!("expected Deliver"),
    }
}
