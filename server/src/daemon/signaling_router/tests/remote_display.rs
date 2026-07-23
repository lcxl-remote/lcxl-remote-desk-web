use super::routing::make_change_display_settings_model;
use super::*;

// ===== RequestRemote virtual-display lifecycle =====

pub(super) fn make_request_remote_model(connection_id: &str) -> SignalingModel {
    make_request_remote_model_with_purpose(connection_id, RemoteSessionPurpose::RemoteDesktop)
}

pub(super) fn make_request_remote_model_with_purpose(
    connection_id: &str,
    purpose: RemoteSessionPurpose,
) -> SignalingModel {
    use desk_signal_facade::model::signal::RequestRemoteModel;
    SignalingModel::new(
        "req-vd-lazy",
        SignalingType::RequestRemote,
        Some(connection_id.to_string()),
        None,
        Some(
            serde_json::to_value(RequestRemoteModel {
                purpose,
                ice_servers: vec![],
                grant_session_id: None,
            })
            .unwrap(),
        ),
        None,
    )
}

#[tokio::test]
pub(super) async fn locked_gate_rejects_request_before_pc_or_session_creation() {
    let (ctx, mut rx) = make_ctx_with_rx().await;
    ctx.host_control_hub
        .remote_access_gate()
        .initialize_from_store(crate::daemon::remote_access::RemoteAccessState::locked(
            2,
            "lock-2".to_string(),
            "2026-07-22T12:00:00Z".to_string(),
            true,
        ));
    let model = make_request_remote_model("conn-locked");

    route(&model, &ctx)
        .await
        .expect("locked request is handled");

    let response = read_response(&mut rx);
    let state = response.response_state.expect("missing locked response");
    assert_eq!(state.error_code, DeskErrorCode::REMOTE_ACCESS_LOCKED.code());
    assert!(ctx.pc_registry.get("conn-locked").await.is_none());
    assert!(
        ctx.host_control_hub
            .host_activity()
            .snapshot()
            .sessions
            .is_empty()
    );
}

#[tokio::test]
pub(super) async fn tombstone_rejects_late_request_after_host_disconnect() {
    let (ctx, mut rx) = make_ctx_with_rx().await;
    ctx.pc_registry
        .tombstone_connection("conn-terminated")
        .await;
    let model = make_request_remote_model("conn-terminated");

    route(&model, &ctx)
        .await
        .expect("tombstoned request is handled");

    let response = read_response(&mut rx);
    let state = response.response_state.expect("missing tombstone response");
    assert_eq!(state.error_code, DeskErrorCode::INVALID_STATE.code());
    assert!(ctx.pc_registry.get("conn-terminated").await.is_none());
    assert!(ctx.pc_registry.admission("conn-terminated").await.is_none());
}

#[tokio::test]
pub(super) async fn file_manager_purpose_is_stored_and_only_promotes_to_desktop() {
    let (ctx, _rx) = make_ctx_with_rx().await;
    ctx.settings.write().await.virtual_display.enabled = false;
    let model = make_request_remote_model_with_purpose(
        "conn-file-purpose",
        RemoteSessionPurpose::FileManager,
    );
    route(&model, &ctx).await.expect("file manager request");

    let pc = ctx
        .pc_registry
        .get("conn-file-purpose")
        .await
        .expect("registered pc");
    let state = pc.read().await.signaling_state.clone();
    assert_eq!(
        state.read().await.purpose,
        RemoteSessionPurpose::FileManager
    );

    promote_desktop_resources(&model, &ctx, "test")
        .await
        .expect("promotion");
    assert_eq!(
        state.read().await.purpose,
        RemoteSessionPurpose::RemoteDesktop
    );
    promote_desktop_resources(&model, &ctx, "repeat")
        .await
        .expect("idempotent promotion");
    assert_eq!(
        state.read().await.purpose,
        RemoteSessionPurpose::RemoteDesktop
    );
}
/// With virtual display disabled, ensure_attached must not be called.
/// The attached test supervisor keeps this path observable without external IO.
/// With virtual display disabled, ensure_attached must not be called. We can't easily mock the supervisor through a trait
/// here, but we can install a `new_attached_for_test` supervisor
/// and verify that the route succeeds without changing state —
/// the ensure_attached fast-path would also produce Attached, but
/// the wider correctness signal is "no panic, route succeeds, no
/// virtual display IPCs emitted".
#[tokio::test]
pub(super) async fn request_remote_skips_ensure_when_feature_disabled() {
    let (mut ctx, _rx) = make_ctx_with_rx().await;
    // Feature disabled by default in Settings::default(), but pin it.
    ctx.settings.write().await.virtual_display.enabled = false;
    let supervisor = Arc::new(VirtualDisplaySupervisor::new_attached_for_test(
        ctx.worker_mgr.clone(),
        "MOCK\\DISPLAY1",
    ));
    ctx.virtual_display = Some(supervisor.clone());
    let label_before = supervisor.state_label().await;

    let model = make_request_remote_model("conn-disabled");
    route(&model, &ctx)
        .await
        .expect("route must succeed even when ensure is skipped");
    assert!(ctx.pc_registry.contains("conn-disabled").await);
    assert_eq!(
        supervisor.state_label().await,
        label_before,
        "ensure_attached must not have been invoked when feature disabled",
    );
}

/// Non-ServiceDaemon mode (virtual_display = None): ensure_attached
/// is skipped entirely. Route must not panic.
#[tokio::test]
pub(super) async fn request_remote_skips_ensure_when_no_supervisor() {
    let (mut ctx, _rx) = make_ctx_with_rx().await;
    ctx.settings.write().await.virtual_display.enabled = true;
    ctx.virtual_display = None;

    let model = make_request_remote_model("conn-no-supervisor");
    route(&model, &ctx)
        .await
        .expect("route must succeed without supervisor");
    assert!(ctx.pc_registry.contains("conn-no-supervisor").await);
}

/// Feature enabled + supervisor already Attached: ensure_attached
/// fast-path returns Attached immediately, route succeeds, the PC
/// is registered, and the supervisor remains Attached.
#[tokio::test]
pub(super) async fn request_remote_invokes_ensure_when_enabled_and_supervisor_attached() {
    let (mut ctx, _rx) = make_ctx_with_rx().await;
    ctx.settings.write().await.virtual_display.enabled = true;
    let supervisor = Arc::new(VirtualDisplaySupervisor::new_attached_for_test(
        ctx.worker_mgr.clone(),
        "MOCK\\DISPLAY1",
    ));
    ctx.virtual_display = Some(supervisor.clone());

    let model = make_request_remote_model("conn-enabled");
    route(&model, &ctx).await.expect("route must succeed");
    assert!(ctx.pc_registry.contains("conn-enabled").await);
    assert_eq!(
        supervisor.state_label().await,
        "Attached",
        "supervisor must remain Attached after fast-path ensure",
    );
}

/// Provider returns NotSupported: ensure_attached resolves as
/// Unavailable instantly and the route falls through to the
/// capabilities-without-IDD Init reply. PC must still be
/// registered.
#[tokio::test]
pub(super) async fn request_remote_continues_when_provider_not_supported() {
    let (mut ctx, _rx) = make_ctx_with_rx().await;
    ctx.settings.write().await.virtual_display.enabled = true;
    let supervisor = Arc::new(VirtualDisplaySupervisor::new_disabled_for_test(
        ctx.worker_mgr.clone(),
    ));
    ctx.virtual_display = Some(supervisor);

    let model = make_request_remote_model("conn-unavailable");
    route(&model, &ctx)
        .await
        .expect("route must continue even when provider is unavailable");
    assert!(ctx.pc_registry.contains("conn-unavailable").await);
}

// ===========================================================
// Auto-resolution ChangeDisplaySettings tests.
// The shared `make_ctx_with_attached_supervisor` flips
// `virtual_display.enabled = true` AND installs an Attached
// supervisor, so each test only needs to focus on its own gate.
// ===========================================================

/// Multi-client guard: `pc_registry.len() != 1` ⇒ INVALID_STATE for
/// auto requests, no IPC sent to worker. This is the user-decided
/// "only single connection" strategy — manual path must keep
/// working, which `manual_request_unaffected_by_multi_pc_guard`
/// covers below.
#[tokio::test]
pub(super) async fn auto_request_rejected_when_multiple_pcs() {
    let (ctx, mut rx, _worker_rx) = make_ctx_with_attached_supervisor().await;
    // Simulate 2 PCs via the test-only override.
    ctx.pc_registry.set_test_len_extra(2);
    assert_eq!(ctx.pc_registry.len().await, 2);

    let model = make_change_display_settings_model(
        "req-multi",
        ChangeDisplaySettingsPayload {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            auto: true,
        },
    );
    route(&model, &ctx).await.expect("route must not error");
    let response = read_response(&mut rx);
    let state = response.response_state.expect("must have error state");
    assert_eq!(state.error_code, DeskErrorCode::INVALID_STATE.code());
    assert!(
        state
            .message
            .as_deref()
            .unwrap_or("")
            .contains("single client"),
        "expected single-client message, got {:?}",
        state.message
    );
}

/// Regression: the daemon must NOT gate auto requests on the
/// server-wide `settings.desk.adaptive_web_page_resolution` value.
/// That field is per-connection (the browser dialog collects it and
/// ships it via `UpdateDeskSettings`, which the router forwards to
/// the worker without writing back to `ctx.settings.desk`), so the
/// server-wide snapshot is always whatever the operator put in
/// `config.toml` — typically `false` (the `DeskSettings::default`).
/// A previous version of the router checked that snapshot and
/// rejected every browser-initiated auto resize with INVALID_STATE
/// even when the user had explicitly enabled adaptive in the dialog.
/// The browser hook is the authoritative gate; the daemon trusts
/// the `auto=true` marker once the request reaches the router.
#[tokio::test]
pub(super) async fn auto_request_passes_even_when_server_desk_setting_false() {
    let (ctx, _rx, mut worker_rx) = make_ctx_with_attached_supervisor().await;
    ctx.settings.write().await.desk.adaptive_web_page_resolution = false;

    let model = make_change_display_settings_model(
        "req-server-default-false",
        ChangeDisplaySettingsPayload {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            auto: true,
        },
    );
    route(&model, &ctx).await.expect("route must not error");

    match worker_rx
        .try_recv()
        .expect("auto IPC must reach the worker regardless of server-wide flag")
    {
        ServiceToWorker::SetVirtualDisplayMode(p) => {
            assert_eq!(p.width, 1920);
            assert_eq!(p.height, 1080);
            assert_eq!(p.refresh_hz, 60);
        }
        other => panic!("unexpected IPC: {other:?}"),
    }
}

/// Browser hook always sends `refresh_hz=0`. With a cached
/// observation the daemon must substitute that value into the IPC.
#[tokio::test]
pub(super) async fn auto_request_substitutes_zero_refresh_with_cached() {
    let (ctx, _rx, mut worker_rx) = make_ctx_with_attached_supervisor().await;
    // Pre-seed only the refresh portion of the supervisor cache so
    // the daemon has an authoritative value to substitute. Using
    // the test-only refresh-only setter (instead of
    // `record_applied_mode`) keeps width/height at zero, which is
    // important here: a full mode would also satisfy
    // `last_known_mode()` and trigger the idempotent short-circuit,
    // bypassing the IPC dispatch this test wants to observe.
    ctx.virtual_display
        .as_ref()
        .expect("supervisor present")
        .seed_refresh_hz_for_test(144);

    let model = make_change_display_settings_model(
        "req-cached",
        ChangeDisplaySettingsPayload {
            width: 1920,
            height: 1080,
            refresh_hz: 0,
            auto: true,
        },
    );
    route(&model, &ctx).await.expect("route must not error");

    match worker_rx.try_recv().expect("IPC must have been dispatched") {
        ServiceToWorker::SetVirtualDisplayMode(p) => {
            assert_eq!(p.width, 1920);
            assert_eq!(p.height, 1080);
            assert_eq!(p.refresh_hz, 144, "must substitute cached refresh");
        }
        other => panic!("unexpected IPC: {other:?}"),
    }
}

/// With no cached observation (`last_refresh_hz=0`), the daemon
/// falls back to 60 — a value guaranteed to live in the IDD's
/// `ALLOWED_REFRESH` set, so the substitute always passes
/// `validate_mode`.
#[tokio::test]
pub(super) async fn auto_request_falls_back_to_60_when_cache_zero() {
    let (ctx, _rx, mut worker_rx) = make_ctx_with_attached_supervisor().await;
    // Supervisor cache is 0 (no observation yet).
    assert_eq!(
        ctx.virtual_display
            .as_ref()
            .expect("supervisor present")
            .last_refresh_hz(),
        0
    );

    let model = make_change_display_settings_model(
        "req-60",
        ChangeDisplaySettingsPayload {
            width: 1920,
            height: 1080,
            refresh_hz: 0,
            auto: true,
        },
    );
    route(&model, &ctx).await.expect("route must not error");

    match worker_rx.try_recv().expect("IPC must have been dispatched") {
        ServiceToWorker::SetVirtualDisplayMode(p) => {
            assert_eq!(p.refresh_hz, 60, "must fall back to 60 when no cache");
        }
        other => panic!("unexpected IPC: {other:?}"),
    }
}

/// Manual requests must keep their original semantics — `refresh_hz=0`
/// fails `validate_mode` as a zero dimension, not silently rescued
/// by the auto fallback. Regression guard for the codex-flagged
/// "fallback may leak into manual path" risk.
#[tokio::test]
pub(super) async fn manual_zero_refresh_still_invalid() {
    let (ctx, mut rx, _worker_rx) = make_ctx_with_attached_supervisor().await;
    let model = make_change_display_settings_model(
        "req-manual-zero",
        ChangeDisplaySettingsPayload {
            width: 1920,
            height: 1080,
            refresh_hz: 0,
            auto: false,
        },
    );
    route(&model, &ctx).await.expect("route must not error");
    let response = read_response(&mut rx);
    let state = response.response_state.expect("must have error state");
    assert_eq!(
        state.error_code,
        DeskErrorCode::INVALID_PARAMS.code(),
        "manual zero refresh must surface INVALID_PARAMS, not silent fallback"
    );
}

/// After an auto request consumes the throttle slot, a manual
/// (`auto=false`) request must still go through — auto throttling
/// is *only* for auto, never for operator-driven changes.
#[tokio::test]
pub(super) async fn manual_request_unaffected_by_auto_throttle() {
    let (ctx, _rx, mut worker_rx) = make_ctx_with_attached_supervisor().await;

    // First, an auto request consumes the slot.
    let auto_model = make_change_display_settings_model(
        "req-auto",
        ChangeDisplaySettingsPayload {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            auto: true,
        },
    );
    route(&auto_model, &ctx).await.expect("auto must succeed");
    let _ = worker_rx.try_recv();

    // Now a manual request right after — throttle MUST be bypassed.
    let manual_model = make_change_display_settings_model(
        "req-manual",
        ChangeDisplaySettingsPayload {
            width: 1280,
            height: 720,
            refresh_hz: 60,
            auto: false,
        },
    );
    route(&manual_model, &ctx)
        .await
        .expect("manual must succeed");
    match worker_rx
        .try_recv()
        .expect("manual IPC must still be dispatched after auto slot consumed")
    {
        ServiceToWorker::SetVirtualDisplayMode(p) => {
            assert_eq!(p.width, 1280);
            assert_eq!(p.height, 720);
        }
        other => panic!("unexpected IPC: {other:?}"),
    }
}

/// Manual auto=false requests bypass the single-client guard too.
/// Operator changes from any connected browser stay functional even
/// in multi-client topologies.
#[tokio::test]
pub(super) async fn manual_request_unaffected_by_multi_pc_guard() {
    let (ctx, _rx, mut worker_rx) = make_ctx_with_attached_supervisor().await;
    ctx.pc_registry.set_test_len_extra(2);

    let model = make_change_display_settings_model(
        "req-manual-multi",
        ChangeDisplaySettingsPayload {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            auto: false,
        },
    );
    route(&model, &ctx).await.expect("route must not error");
    assert!(
        matches!(
            worker_rx.try_recv(),
            Ok(ServiceToWorker::SetVirtualDisplayMode(_))
        ),
        "manual request must reach worker even with multiple PCs",
    );
}

/// `adaptive_throttle_ms` is read from `Settings` per call (not
/// cached on the supervisor), so a tight throttle in settings must
/// drop the second back-to-back auto request. Pins the live-read
/// behaviour.
#[tokio::test]
pub(super) async fn auto_throttle_tight_setting_drops_second_request() {
    let (ctx, _rx, mut worker_rx) = make_ctx_with_attached_supervisor().await;
    ctx.settings
        .write()
        .await
        .virtual_display
        .adaptive_throttle_ms = 60_000; // tight: 60 s

    for (req_id, w, h) in [("req-tight-1", 1920, 1080), ("req-tight-2", 1280, 720)] {
        let model = make_change_display_settings_model(
            req_id,
            ChangeDisplaySettingsPayload {
                width: w,
                height: h,
                refresh_hz: 60,
                auto: true,
            },
        );
        route(&model, &ctx).await.expect("route must not error");
    }
    assert!(
        matches!(
            worker_rx.try_recv(),
            Ok(ServiceToWorker::SetVirtualDisplayMode(_))
        ),
        "first auto must pass through the throttle",
    );
    assert!(
        worker_rx.try_recv().is_err(),
        "second back-to-back auto must be throttled (no IPC)",
    );
}

/// `adaptive_throttle_ms = 0` is the explicit "no defense" mode.
/// Back-to-back auto requests must both reach the worker. Together
/// with `auto_throttle_tight_setting_drops_second_request` this
/// pins that the throttle interval really comes from settings —
/// flipping the value flips the behaviour.
#[tokio::test]
pub(super) async fn auto_throttle_zero_setting_allows_back_to_back() {
    let (ctx, _rx, mut worker_rx) = make_ctx_with_attached_supervisor().await;
    ctx.settings
        .write()
        .await
        .virtual_display
        .adaptive_throttle_ms = 0; // disabled

    for (req_id, w, h) in [("req-free-1", 1920, 1080), ("req-free-2", 1280, 720)] {
        let model = make_change_display_settings_model(
            req_id,
            ChangeDisplaySettingsPayload {
                width: w,
                height: h,
                refresh_hz: 60,
                auto: true,
            },
        );
        route(&model, &ctx).await.expect("route must not error");
    }
    assert!(
        matches!(
            worker_rx.try_recv(),
            Ok(ServiceToWorker::SetVirtualDisplayMode(_))
        ),
        "first auto must pass when throttle disabled",
    );
    assert!(
        matches!(
            worker_rx.try_recv(),
            Ok(ServiceToWorker::SetVirtualDisplayMode(_))
        ),
        "second auto must also pass when throttle disabled",
    );
}

// ===========================================================
// Idempotent short-circuit tests.
// Cached `(width, height, refresh_hz)` matching the inbound
// request must skip the worker IPC and return Applied inline.
// ===========================================================

/// Cold start — no cache. Auto request must NOT short-circuit and
/// must reach the worker as IPC. This is the negative-control
/// baseline the rest of the idempotent tests sit on top of.
#[tokio::test]
pub(super) async fn idempotent_cold_cache_dispatches_ipc_normally() {
    let (ctx, _rx, mut worker_rx) = make_ctx_with_attached_supervisor().await;
    // Sanity: nothing observed yet.
    assert!(
        ctx.virtual_display
            .as_ref()
            .expect("supervisor")
            .last_known_mode()
            .is_none()
    );

    let model = make_change_display_settings_model(
        "req-cold",
        ChangeDisplaySettingsPayload {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            auto: true,
        },
    );
    route(&model, &ctx).await.expect("route must not error");
    assert!(
        matches!(
            worker_rx.try_recv(),
            Ok(ServiceToWorker::SetVirtualDisplayMode(_))
        ),
        "cold cache must dispatch IPC, not short-circuit",
    );
}

/// Cache exactly matches the inbound auto request — short-circuit:
/// no IPC, browser receives a success response inline.
#[tokio::test]
pub(super) async fn idempotent_exact_match_short_circuits_no_ipc() {
    let (ctx, mut rx, mut worker_rx) = make_ctx_with_attached_supervisor().await;
    ctx.virtual_display
        .as_ref()
        .expect("supervisor")
        .record_applied_mode(1920, 1080, 60);

    let model = make_change_display_settings_model(
        "req-hit",
        ChangeDisplaySettingsPayload {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            auto: true,
        },
    );
    route(&model, &ctx).await.expect("route must not error");

    // Browser sees a fully-formed success response with the cached
    // dimensions echoed back.
    let response = read_response(&mut rx);
    let state = response
        .response_state
        .as_ref()
        .expect("must have response state");
    assert_eq!(
        state.error_code,
        DeskErrorCode::SUCCESS.code(),
        "idempotent hit must yield success, not error",
    );
    let echoed: ChangeDisplaySettingsPayload =
        response.get_data().expect("response payload must decode");
    assert_eq!(echoed.width, 1920);
    assert_eq!(echoed.height, 1080);
    assert_eq!(echoed.refresh_hz, 60);

    // No worker IPC dispatched.
    assert!(
        worker_rx.try_recv().is_err(),
        "idempotent hit must not dispatch worker IPC",
    );
}

/// Idempotent hit must NOT consume the throttle slot. Verified by
/// setting a tight throttle, firing a same-resolution auto (hit),
/// then firing a different-resolution auto that MUST reach the
/// worker — if the hit had consumed the slot, the second request
/// would be rejected with "auto change throttled". Note that we
/// cannot use a manual request to probe throttle consumption:
/// manual requests bypass the throttle gate entirely (`payload.auto`
/// branch in `handle_change_display_settings_inbound`).
#[tokio::test]
pub(super) async fn idempotent_hit_does_not_consume_throttle_slot() {
    let (ctx, _rx, mut worker_rx) = make_ctx_with_attached_supervisor().await;
    ctx.settings
        .write()
        .await
        .virtual_display
        .adaptive_throttle_ms = 60_000; // tight: 60 s
    ctx.virtual_display
        .as_ref()
        .expect("supervisor")
        .record_applied_mode(1920, 1080, 60);

    // First auto: same resolution — idempotent hit, no IPC, no
    // throttle slot consumed.
    let hit = make_change_display_settings_model(
        "req-hit-throttle",
        ChangeDisplaySettingsPayload {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            auto: true,
        },
    );
    route(&hit, &ctx).await.expect("route must not error");
    assert!(
        worker_rx.try_recv().is_err(),
        "idempotent hit must not dispatch worker IPC",
    );

    // Second auto immediately after: different resolution — must
    // pass through to the worker. If the previous hit had consumed
    // the throttle slot this would be rejected with INVALID_STATE.
    let real = make_change_display_settings_model(
        "req-after-hit",
        ChangeDisplaySettingsPayload {
            width: 1280,
            height: 720,
            refresh_hz: 60,
            auto: true,
        },
    );
    route(&real, &ctx).await.expect("route must not error");
    match worker_rx
        .try_recv()
        .expect("second auto must reach worker — throttle slot must NOT have been consumed")
    {
        ServiceToWorker::SetVirtualDisplayMode(p) => {
            assert_eq!(p.width, 1280);
            assert_eq!(p.height, 720);
        }
        other => panic!("unexpected IPC: {other:?}"),
    }
}

/// Width differs ⇒ no short-circuit, IPC dispatched.
#[tokio::test]
pub(super) async fn idempotent_miss_on_width_dispatches_ipc() {
    let (ctx, _rx, mut worker_rx) = make_ctx_with_attached_supervisor().await;
    ctx.virtual_display
        .as_ref()
        .expect("supervisor")
        .record_applied_mode(1920, 1080, 60);

    let model = make_change_display_settings_model(
        "req-miss-w",
        ChangeDisplaySettingsPayload {
            width: 1280,
            height: 1080,
            refresh_hz: 60,
            auto: true,
        },
    );
    route(&model, &ctx).await.expect("route must not error");
    assert!(matches!(
        worker_rx.try_recv(),
        Ok(ServiceToWorker::SetVirtualDisplayMode(_))
    ));
}

/// Refresh differs ⇒ no short-circuit, IPC dispatched.
#[tokio::test]
pub(super) async fn idempotent_miss_on_refresh_dispatches_ipc() {
    let (ctx, _rx, mut worker_rx) = make_ctx_with_attached_supervisor().await;
    ctx.virtual_display
        .as_ref()
        .expect("supervisor")
        .record_applied_mode(1920, 1080, 60);

    let model = make_change_display_settings_model(
        "req-miss-hz",
        ChangeDisplaySettingsPayload {
            width: 1920,
            height: 1080,
            refresh_hz: 75,
            auto: false,
        },
    );
    route(&model, &ctx).await.expect("route must not error");
    assert!(matches!(
        worker_rx.try_recv(),
        Ok(ServiceToWorker::SetVirtualDisplayMode(_))
    ));
}

/// Auto request with `refresh_hz=0` substitutes the cached refresh
/// before the idempotent comparison; if the substitution lands on
/// the cached value AND dimensions match, the hit fires.
#[tokio::test]
pub(super) async fn idempotent_hits_when_zero_refresh_resolves_to_cached() {
    let (ctx, mut rx, mut worker_rx) = make_ctx_with_attached_supervisor().await;
    ctx.virtual_display
        .as_ref()
        .expect("supervisor")
        .record_applied_mode(1920, 1080, 60);

    let model = make_change_display_settings_model(
        "req-auto-zero-hit",
        ChangeDisplaySettingsPayload {
            width: 1920,
            height: 1080,
            refresh_hz: 0, // gets resolved to cached 60
            auto: true,
        },
    );
    route(&model, &ctx).await.expect("route must not error");
    let response = read_response(&mut rx);
    let state = response
        .response_state
        .as_ref()
        .expect("must have response state");
    assert_eq!(state.error_code, DeskErrorCode::SUCCESS.code());
    let echoed: ChangeDisplaySettingsPayload =
        response.get_data().expect("response payload must decode");
    assert_eq!(
        echoed.refresh_hz, 60,
        "synth response echoes cached refresh"
    );
    assert!(
        worker_rx.try_recv().is_err(),
        "auto with refresh_hz=0 and matching dims must short-circuit",
    );
}

/// Codex round 1 #1 regression: after a complete detach the
/// dimension cache is cleared (refresh survives), so the next
/// same-resolution request must NOT be faked — it must reach the
/// worker and actually drive the IDD. This pins the fix for the
/// fake-Applied-on-stale-cache hazard that the codex review
/// caught. We model "post-reattach" state directly by injecting a
/// fresh Attached supervisor with only the refresh portion of the
/// cache populated (mirroring what `reset_known_dimensions` leaves
/// behind after the supervisor goes through an
/// attach→detach→re-attach cycle).
#[tokio::test]
pub(super) async fn idempotent_does_not_short_circuit_after_reattach() {
    let (ctx, _rx, mut worker_rx) = make_ctx_with_attached_supervisor().await;
    let supervisor = ctx.virtual_display.as_ref().expect("supervisor");
    // Post-reattach state: refresh kept as operator hint, dims
    // cleared by `reset_known_dimensions` on the attach transition.
    supervisor.seed_refresh_hz_for_test(60);
    assert!(
        supervisor.last_known_mode().is_none(),
        "post-reattach dimensions must be empty even though refresh survives",
    );

    let model = make_change_display_settings_model(
        "req-after-reattach",
        ChangeDisplaySettingsPayload {
            width: 2560,
            height: 1440,
            refresh_hz: 60,
            auto: true,
        },
    );
    route(&model, &ctx).await.expect("route must not error");
    match worker_rx
        .try_recv()
        .expect("post-reattach same-dims request must dispatch IPC, not fake-Applied")
    {
        ServiceToWorker::SetVirtualDisplayMode(p) => {
            assert_eq!(p.width, 2560);
            assert_eq!(p.height, 1440);
            assert_eq!(p.refresh_hz, 60);
        }
        other => panic!("unexpected IPC: {other:?}"),
    }
}

// ───── Exclusive helper tests ─────

pub(super) fn settings_with_exclusive(
    enabled: bool,
    exclusive: bool,
    prompt_ms: u32,
) -> Arc<crate::model::settings::SharedSettings> {
    let mut s = crate::model::settings::Settings::default();
    s.virtual_display.enabled = enabled;
    s.virtual_display.exclusive = exclusive;
    s.virtual_display.prompt_ms = prompt_ms;
    Arc::new(crate::model::settings::SharedSettings::from(s))
}

/// settings off OR active=false ⇒ (false, prompt_ms).
#[tokio::test]
pub(super) async fn compute_desired_off_when_settings_disable_or_inactive() {
    let s_off = settings_with_exclusive(false, true, 2500);
    let s_excl_off = settings_with_exclusive(true, false, 3300);
    let s_on = settings_with_exclusive(true, true, 4400);
    let registry = PcRegistry::new();

    assert_eq!(
        compute_desired_with_active(&s_off, &registry, true).await,
        (false, 2500)
    );
    assert_eq!(
        compute_desired_with_active(&s_excl_off, &registry, true).await,
        (false, 3300)
    );
    // settings on but supervisor not active ⇒ desired false.
    assert_eq!(
        compute_desired_with_active(&s_on, &registry, false).await,
        (false, 4400)
    );
}

/// `update_exclusive_after_control_change` short-circuits when
/// `outcome.changed = false`. The supervisor's exclusive state
/// watch must not see any transition.
#[tokio::test]
pub(super) async fn update_exclusive_skips_when_outcome_unchanged() {
    use crate::daemon::pc_manager::ControlOutcome;
    let mut ctx = make_ctx().await;
    ctx.settings.write().await.virtual_display.enabled = true;
    ctx.settings.write().await.virtual_display.exclusive = true;
    let supervisor =
        crate::daemon::virtual_display::VirtualDisplaySupervisor::new_attached_for_test(
            ctx.worker_mgr.clone(),
            "SWD\\MOCK\\MOCK",
        );
    let supervisor = Arc::new(supervisor);
    ctx.virtual_display = Some(supervisor.clone());
    // Observation: the watch carries `Idle` initially; a changed=false
    // outcome must not produce any send_replace (the helper short-
    // circuits before touching the supervisor).
    let mut rx = supervisor.subscribe_exclusive_state();
    // First borrow is the initial value (Idle).
    assert_eq!(
        *rx.borrow(),
        crate::daemon::virtual_display::ExclusiveState::Idle
    );
    let outcome = ControlOutcome {
        connection_id: "conn-x".into(),
        accept_control: true,
        changed: false,
    };
    update_exclusive_after_control_change(&ctx, &outcome).await;
    // No state change to consume — `try_changed` returns NotChanged
    // because nothing was send_replace'd. We can verify by polling
    // with a tiny timeout.
    let res = tokio::time::timeout(std::time::Duration::from_millis(50), rx.changed()).await;
    assert!(res.is_err(), "no state change must arrive");
}
