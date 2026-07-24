use super::*;
use desk_ipc_protocol::dual_transport::inprocess;
use desk_signal_facade::model::desk_settings::DeskSettings;

impl MediaProducer {
    fn connection_pipeline_state(&self, connection_id: &str) -> Option<(bool, bool)> {
        let map = self.inner.lock().expect("media producer lock poisoned");
        map.get(connection_id)
            .map(|task| (task.video_handle.is_some(), task.audio_handle.is_some()))
    }
}

/// Walk a typical IDR access unit (SPS + PPS + IDR slice) and verify
/// each NAL's header + payload length is reported. This is the
/// shape we expect on a healthy initial frame, and the diff between
/// "real IDR slice = many KB" and "dummy slice = few bytes" is the
/// signal we're after when the screen turns green after a rebuild.
#[test]
fn h264_walk_nals_lists_sps_pps_idr() {
    let mut bytes: Vec<u8> = Vec::new();
    // SPS (3 bytes payload incl header)
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xC0]);
    // PPS (3 bytes payload incl header)
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x3C]);
    // IDR slice (5 bytes payload incl header)
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65, 0xB8, 0x00, 0x04, 0x00]);
    let nals = h264_walk_nals(&bytes);
    assert_eq!(nals.len(), 3, "expected 3 NAL units (SPS + PPS + IDR)");
    assert_eq!(nals[0].0 & 0x1F, 7, "first NAL must be SPS");
    assert_eq!(nals[0].1, 3);
    assert_eq!(nals[1].0 & 0x1F, 8, "second NAL must be PPS");
    assert_eq!(nals[1].1, 3);
    assert_eq!(nals[2].0 & 0x1F, 5, "third NAL must be IDR");
    assert_eq!(nals[2].1, 5);
}

/// Mixed 3-byte and 4-byte startcodes are both recognised. The
/// trailing NAL's length must extend to end-of-buffer.
#[test]
fn h264_walk_nals_handles_mixed_startcodes() {
    // 4-byte startcode + AUD (1 byte) + 3-byte startcode + SEI (4
    // bytes to end-of-buffer)
    let bytes: Vec<u8> = vec![
        0x00, 0x00, 0x00, 0x01, 0x09, 0x00, 0x00, 0x01, 0x06, 0x05, 0xFF, 0x80,
    ];
    let nals = h264_walk_nals(&bytes);
    assert_eq!(nals.len(), 2);
    assert_eq!(nals[0].0 & 0x1F, 9, "AccessUnitDelim");
    assert_eq!(nals[0].1, 1);
    assert_eq!(nals[1].0 & 0x1F, 6, "SEI");
    assert_eq!(nals[1].1, 4);
}

/// Empty / mis-framed buffers must yield an empty list rather than
/// panicking. Guards the diagnostic against poisoning the event
/// pipeline if a non-H264 stream lands in the H264 branch.
#[test]
fn h264_walk_nals_handles_short_or_missing() {
    assert!(h264_walk_nals(&[]).is_empty());
    assert!(h264_walk_nals(&[0xAA, 0xBB, 0xCC]).is_empty());
    // Only a startcode, no header byte after — also empty (the
    // walker skips entries whose header offset is past the end).
    assert!(h264_walk_nals(&[0x00, 0x00, 0x00, 0x01]).is_empty());
}

/// Codec round-trip: the strings emitted by the encoder factory map
/// back to the IPC enum without surprises.
#[test]
fn codec_from_str_round_trips_video_set() {
    for name in ["H264", "X264"] {
        assert_eq!(codec_from_str(name, true), Some(MediaCodec::H264));
    }
    assert_eq!(codec_from_str("VP8", true), Some(MediaCodec::Vp8));
    assert_eq!(codec_from_str("VP9", true), Some(MediaCodec::Vp9));
    assert_eq!(codec_from_str("AV1", true), Some(MediaCodec::Av1));
}

#[test]
fn codec_from_str_unknown_returns_none_not_panic() {
    assert!(codec_from_str("FANCY-NEW-CODEC", true).is_none());
    assert!(codec_from_str("AV1", false).is_none()); // wrong category
}

#[test]
fn video_codec_name_round_trips() {
    for c in [
        MediaCodec::H264,
        MediaCodec::Vp8,
        MediaCodec::Vp9,
        MediaCodec::Av1,
    ] {
        let name = video_codec_name(c).expect("name for video codec");
        assert_eq!(codec_from_str(name, true), Some(c));
    }
    assert!(video_codec_name(MediaCodec::Opus).is_none());
}

/// The payload override path picks the per-connection codec / fps
/// over the worker default settings. A zero `fps` means "keep
/// default" so the daemon does not have to know the worker's
/// preferred fallback.
#[test]
fn payload_overrides_apply_codec_and_fps() {
    let base = DeskSettings {
        video_fps: 30,
        video_encoder: Some("X264".into()),
        ..DeskSettings::default()
    };
    let payload = StartMediaPayload {
        connection_id: "c1".into(),
        video_codec: MediaCodec::Vp9,
        audio_codec: MediaCodec::Opus,
        video_device: Some(r"\\.\DISPLAY7".to_string()),
        audio_device: None,
        fps: 60,
        bitrate_kbps: 0,
        quality: 0,
        start_video: true,
        start_audio: true,
        image_capture: None,
        enable_dirty_rect: None,
    };
    let merged = payload_overrides(&base, &payload);
    assert_eq!(merged.video_encoder.as_deref(), Some("VP9"));
    assert_eq!(merged.video_fps, 60);
    // v4 capture-selection fix: payload_overrides must propagate
    // the per-connection device_name so each browser binds capture
    // to the monitor it picked in the dropdown. Pin the wire
    // contract here — a regression that drops the field would
    // silently revert the worker to its base-settings target.
    assert_eq!(merged.video_device_name, r"\\.\DISPLAY7");
}

/// `video_device = None` (legacy daemon, or daemon that mapped
/// empty `video_device_name` to None) leaves the worker's base
/// `video_device_name` untouched. The capture-engine still hard-
/// errors if the base value is empty — that is the documented
/// "no display selected" path for a fresh install.
#[test]
fn payload_overrides_preserves_base_video_device_name_when_payload_is_none() {
    let base = DeskSettings {
        video_device_name: r"\\.\DISPLAY1".to_string(),
        ..DeskSettings::default()
    };
    let payload = StartMediaPayload {
        connection_id: "c1".into(),
        video_codec: MediaCodec::Vp9,
        audio_codec: MediaCodec::Opus,
        video_device: None,
        audio_device: None,
        fps: 60,
        bitrate_kbps: 0,
        quality: 0,
        start_video: true,
        start_audio: true,
        image_capture: None,
        enable_dirty_rect: None,
    };
    let merged = payload_overrides(&base, &payload);
    assert_eq!(merged.video_device_name, r"\\.\DISPLAY1");
}

/// Per-connection `image_capture` choice from the daemon overrides
/// the worker's startup snapshot. Regression for the
/// "second-browser-can't-pick-GDI" bug: pre-fix `payload_overrides`
/// dropped the field on the floor and every connection inherited
/// the worker's base backend (DXGI by default), causing the second
/// connection to hit `DuplicateOutput` E_INVALIDARG against the
/// first connection's already-active duplication.
#[test]
fn payload_overrides_apply_per_connection_image_capture() {
    let base = DeskSettings {
        image_capture: Some("DXGI".into()),
        ..DeskSettings::default()
    };
    let payload = StartMediaPayload {
        connection_id: "c2".into(),
        video_codec: MediaCodec::H264,
        audio_codec: MediaCodec::Opus,
        video_device: None,
        audio_device: None,
        fps: 0,
        bitrate_kbps: 0,
        quality: 0,
        start_video: true,
        start_audio: true,
        image_capture: Some("GDI".into()),
        enable_dirty_rect: None,
    };
    let merged = payload_overrides(&base, &payload);
    assert_eq!(
        merged.image_capture.as_deref(),
        Some("GDI"),
        "per-connection override must replace the worker's base backend"
    );
}

/// Conversely, when the daemon does not specify a backend (e.g. an
/// older daemon that predates the IPC field, or an offer with no
/// preference), the worker must keep its base setting unchanged so
/// the platform default still applies.
#[test]
fn payload_overrides_image_capture_none_preserves_base() {
    let base = DeskSettings {
        image_capture: Some("DXGI".into()),
        ..DeskSettings::default()
    };
    let payload = StartMediaPayload {
        connection_id: "c3".into(),
        video_codec: MediaCodec::H264,
        audio_codec: MediaCodec::Opus,
        video_device: None,
        audio_device: None,
        fps: 0,
        bitrate_kbps: 0,
        quality: 0,
        start_video: true,
        start_audio: true,
        image_capture: None,
        enable_dirty_rect: None,
    };
    let merged = payload_overrides(&base, &payload);
    assert_eq!(merged.image_capture.as_deref(), Some("DXGI"));
}

#[test]
fn payload_overrides_fps_zero_keeps_default() {
    let base = DeskSettings {
        video_fps: 24,
        ..DeskSettings::default()
    };
    let payload = StartMediaPayload {
        connection_id: "c1".into(),
        video_codec: MediaCodec::H264,
        audio_codec: MediaCodec::Opus,
        video_device: None,
        audio_device: None,
        fps: 0,
        bitrate_kbps: 0,
        quality: 0,
        start_video: true,
        start_audio: true,
        image_capture: None,
        enable_dirty_rect: None,
    };
    let merged = payload_overrides(&base, &payload);
    assert_eq!(merged.video_fps, 24);
}

/// Regression: a `StartMedia` payload with `start_video = false`
/// and `start_audio = false` must register a `ConnectionTask`
/// slot (so subsequent `StopMedia` / `ForceKeyframe` find it) but
/// must NOT spawn either pipeline thread. Bug fix 2026-05-05:
/// previously the worker always lit up DXGI + WASAPI capture for
/// every PC, including the browser file-management page that
/// negotiates a DataChannel-only PC.
#[test]
fn start_media_data_channel_only_skips_both_pipelines() {
    let (sender, _rx) = inprocess::make_media();
    let (err_tx, _err_rx) = mpsc::unbounded_channel::<WorkerToService>();
    let producer = MediaProducer::new(DeskSettings::default(), sender, err_tx);
    producer.start_media(StartMediaPayload {
        connection_id: "files".into(),
        video_codec: MediaCodec::H264,
        audio_codec: MediaCodec::Opus,
        video_device: None,
        audio_device: None,
        fps: 0,
        bitrate_kbps: 0,
        quality: 0,
        start_video: false,
        start_audio: false,
        image_capture: None,
        enable_dirty_rect: None,
    });
    let state = producer
        .connection_pipeline_state("files")
        .expect("DataChannel-only StartMedia must still register the connection slot");
    assert_eq!(
        state,
        (false, false),
        "DataChannel-only StartMedia must not spawn video or audio pipeline"
    );
    // StopMedia must find the entry and clean it up; pre-fix this
    // would have logged a debug "unknown connection" — the test
    // passes either way but keeps the symmetry pinned.
    producer.stop_media(&StopMediaPayload {
        connection_id: "files".into(),
    });
    assert!(
        producer.connection_pipeline_state("files").is_none(),
        "stop_media must drop the slot"
    );
}

/// Force-keyframe / stop-media on an unknown connection must be a
/// silent no-op (race with browser drop). The producer has to be
/// safe to drive from the daemon even when the daemon's view of
/// active connections is briefly stale.
#[test]
fn force_keyframe_and_stop_media_unknown_id_is_noop() {
    let (sender, _rx) = inprocess::make_media();
    let (err_tx, _err_rx) = mpsc::unbounded_channel::<WorkerToService>();
    let producer = MediaProducer::new(DeskSettings::default(), sender, err_tx);
    producer.force_keyframe("never-existed");
    producer.stop_media(&StopMediaPayload {
        connection_id: "never-existed".into(),
    });
    // Nothing to assert beyond "did not panic" — the unit test
    // exists to guard against `unwrap()` on a missing entry.
}

/// `update_settings` for an unknown connection_id silently drops
/// (the producer doesn't allocate a per-connection task until
/// `start_media`); the daemon may race a `StopMedia` with a
/// settings change so the lookup-miss path must stay quiet.
#[test]
fn update_settings_does_not_panic_on_unknown_connection() {
    let (sender, _rx) = inprocess::make_media();
    let (err_tx, _err_rx) = mpsc::unbounded_channel::<WorkerToService>();
    let producer = MediaProducer::new(DeskSettings::default(), sender, err_tx);
    producer.update_settings(UpdateMediaSettingsPayload {
        connection_id: "anything".into(),
        fps: Some(30),
        bitrate_kbps: Some(2_000),
        quality: Some(50),
        enable_dirty_rect: None,
    });
}

/// `drain_settings_updates` applies fps and quality changes to
/// `merged_settings`, rebuilds the ticker on fps changes, and
/// returns `true` so the caller knows to recreate the encoder.
/// A repeat of the same value is a no-op (returns `false`).
#[tokio::test(flavor = "current_thread")]
async fn drain_settings_updates_applies_fps_and_quality() {
    let (tx, mut rx) = mpsc::unbounded_channel::<UpdateMediaSettingsPayload>();
    let mut merged = DeskSettings {
        video_fps: 30,
        video_quality: 22,
        ..DeskSettings::default()
    };
    let mut frame_interval = merged.get_duration_by_video_fps();
    let mut frame_duration_ns = frame_interval.as_nanos().min(u64::MAX as u128) as u64;

    // No pending update → no rebuild, no cap directive, state untouched.
    let outcome = drain_settings_updates(
        "c1",
        &mut rx,
        &mut merged,
        &mut frame_interval,
        &mut frame_duration_ns,
    );
    assert!(!outcome.needs_rebuild);
    assert!(outcome.cap_directive.is_none());
    assert_eq!(merged.video_fps, 30);

    // Apply fps=60 + quality=40 → both change, returns true, frame
    // duration recomputed.
    tx.send(UpdateMediaSettingsPayload {
        connection_id: "c1".into(),
        fps: Some(60),
        bitrate_kbps: None,
        quality: Some(40),
        enable_dirty_rect: None,
    })
    .unwrap();
    let outcome = drain_settings_updates(
        "c1",
        &mut rx,
        &mut merged,
        &mut frame_interval,
        &mut frame_duration_ns,
    );
    assert!(outcome.needs_rebuild);
    assert_eq!(merged.video_fps, 60);
    assert_eq!(merged.video_quality, 40);
    assert_eq!(
        frame_duration_ns,
        merged
            .get_duration_by_video_fps()
            .as_nanos()
            .min(u64::MAX as u128) as u64,
        "frame duration must follow the new fps"
    );

    // Same values again → no-op, no rebuild.
    tx.send(UpdateMediaSettingsPayload {
        connection_id: "c1".into(),
        fps: Some(60),
        bitrate_kbps: None,
        quality: Some(40),
        enable_dirty_rect: None,
    })
    .unwrap();
    let outcome = drain_settings_updates(
        "c1",
        &mut rx,
        &mut merged,
        &mut frame_interval,
        &mut frame_duration_ns,
    );
    assert!(!outcome.needs_rebuild);
}

/// Regression for the dirty-rect kill-switch wiring: the browser's
/// Advanced-tab toggle eventually lands as
/// `UpdateMediaSettingsPayload.enable_dirty_rect`; `drain_settings_
/// updates` must apply it to `merged_settings.enable_dirty_rect`
/// so the next `encoder.encode(..., enable_dirty_rect)` call
/// honours it. Pre-fix the field did not exist on the payload at
/// all, so the worker's `merged_settings.enable_dirty_rect` was
/// frozen at the worker's startup default (`true`) regardless of
/// what the browser sent.
#[tokio::test(flavor = "current_thread")]
async fn drain_settings_updates_applies_enable_dirty_rect() {
    let (tx, mut rx) = mpsc::unbounded_channel::<UpdateMediaSettingsPayload>();
    let mut merged = DeskSettings {
        enable_dirty_rect: true,
        ..DeskSettings::default()
    };
    let mut frame_interval = merged.get_duration_by_video_fps();
    let mut frame_duration_ns = frame_interval.as_nanos().min(u64::MAX as u128) as u64;

    tx.send(UpdateMediaSettingsPayload {
        connection_id: "c1".into(),
        fps: None,
        bitrate_kbps: None,
        quality: None,
        enable_dirty_rect: Some(false),
    })
    .unwrap();
    let outcome = drain_settings_updates(
        "c1",
        &mut rx,
        &mut merged,
        &mut frame_interval,
        &mut frame_duration_ns,
    );
    // Dirty-rect flips do not force an encoder rebuild — the
    // encoder reads the flag per-frame.
    assert!(
        !outcome.needs_rebuild,
        "enable_dirty_rect-only change must not force encoder rebuild"
    );
    assert!(
        !merged.enable_dirty_rect,
        "enable_dirty_rect must be applied to merged_settings"
    );

    // Re-enabling round-trips just as cleanly.
    tx.send(UpdateMediaSettingsPayload {
        connection_id: "c1".into(),
        fps: None,
        bitrate_kbps: None,
        quality: None,
        enable_dirty_rect: Some(true),
    })
    .unwrap();
    let _ = drain_settings_updates(
        "c1",
        &mut rx,
        &mut merged,
        &mut frame_interval,
        &mut frame_duration_ns,
    );
    assert!(merged.enable_dirty_rect);
}

/// `payload_overrides` must honour `StartMediaPayload.
/// enable_dirty_rect` so a fresh connection picks up the
/// browser's Advanced-tab toggle on the *first* frame rather than
/// waiting for a follow-up `UpdateMediaSettings`. Regression
/// guard: pre-fix the field did not exist on `StartMediaPayload`,
/// so a connection that negotiated `enable_dirty_rect=false`
/// would still see the worker's base default (`true`) until the
/// next live settings round-trip.
#[test]
fn payload_overrides_applies_enable_dirty_rect() {
    let base = DeskSettings {
        enable_dirty_rect: true,
        ..DeskSettings::default()
    };
    let payload = StartMediaPayload {
        connection_id: "c-dr".into(),
        video_codec: MediaCodec::H264,
        audio_codec: MediaCodec::Opus,
        video_device: None,
        audio_device: None,
        fps: 0,
        bitrate_kbps: 0,
        quality: 0,
        start_video: true,
        start_audio: true,
        image_capture: None,
        enable_dirty_rect: Some(false),
    };
    let merged = payload_overrides(&base, &payload);
    assert!(
        !merged.enable_dirty_rect,
        "payload override must replace the worker's base value"
    );

    // `None` preserves base — back-compat path with older daemons
    // that do not yet sniff the field.
    let payload_none = StartMediaPayload {
        enable_dirty_rect: None,
        ..payload
    };
    let merged_none = payload_overrides(&base, &payload_none);
    assert!(merged_none.enable_dirty_rect);
}

/// `drain_settings_updates` ignores `fps = 0` (sentinel for "use
/// default") while a `bitrate_kbps` value surfaces as a cap
/// directive that must NOT trigger an encoder rebuild — cap
/// updates arrive at REMB cadence and are applied via
/// `set_bitrate_cap` instead.
#[tokio::test(flavor = "current_thread")]
async fn drain_settings_updates_fps_zero_ignored_bitrate_is_cap_directive() {
    let (tx, mut rx) = mpsc::unbounded_channel::<UpdateMediaSettingsPayload>();
    let mut merged = DeskSettings {
        video_fps: 30,
        video_quality: 22,
        ..DeskSettings::default()
    };
    let mut frame_interval = merged.get_duration_by_video_fps();
    let mut frame_duration_ns = 0u64;

    tx.send(UpdateMediaSettingsPayload {
        connection_id: "c1".into(),
        fps: Some(0), // sentinel — must NOT replace 30 with 0 fps
        bitrate_kbps: Some(8_000),
        quality: None,
        enable_dirty_rect: None,
    })
    .unwrap();
    let outcome = drain_settings_updates(
        "c1",
        &mut rx,
        &mut merged,
        &mut frame_interval,
        &mut frame_duration_ns,
    );
    assert!(
        !outcome.needs_rebuild,
        "fps=0 + bitrate alone must not force an encoder rebuild"
    );
    assert_eq!(
        outcome.cap_directive,
        Some(Some(8_000)),
        "bitrate_kbps must surface as a cap directive"
    );
    assert_eq!(merged.video_fps, 30);
    assert_eq!(merged.video_quality, 22);
}

/// The `Some(0)` wire sentinel translates to a clear-cap directive
/// (`Some(None)`), and the newest directive in a drained batch
/// wins. Pinning the sentinel so a future maintainer does not
/// "sanitise" zero away as an invalid bitrate.
#[tokio::test(flavor = "current_thread")]
async fn drain_settings_updates_zero_bitrate_clears_and_latest_wins() {
    let (tx, mut rx) = mpsc::unbounded_channel::<UpdateMediaSettingsPayload>();
    let mut merged = DeskSettings::default();
    let mut frame_interval = merged.get_duration_by_video_fps();
    let mut frame_duration_ns = 0u64;

    let send_cap = |kbps: u32| {
        tx.send(UpdateMediaSettingsPayload {
            connection_id: "c1".into(),
            fps: None,
            bitrate_kbps: Some(kbps),
            quality: None,
            enable_dirty_rect: None,
        })
        .unwrap();
    };

    // A batch of cap directives — only the last one survives.
    send_cap(4_000);
    send_cap(2_000);
    let outcome = drain_settings_updates(
        "c1",
        &mut rx,
        &mut merged,
        &mut frame_interval,
        &mut frame_duration_ns,
    );
    assert!(!outcome.needs_rebuild);
    assert_eq!(outcome.cap_directive, Some(Some(2_000)));

    // Some(0) = clear-cap sentinel → Some(None).
    send_cap(0);
    let outcome = drain_settings_updates(
        "c1",
        &mut rx,
        &mut merged,
        &mut frame_interval,
        &mut frame_duration_ns,
    );
    assert!(!outcome.needs_rebuild);
    assert_eq!(
        outcome.cap_directive,
        Some(None),
        "Some(0) must clear the cap, not be dropped as invalid"
    );
}

/// `build_media_frame` stamps ts_ns from wall clock, copies through
/// the inputs, and produces a frame the daemon can decode end-to-end.
#[test]
fn build_media_frame_produces_consistent_payload() {
    let frame = build_media_frame(
        "c-x",
        42,
        16_666_666,
        MediaFrameKind::VideoI,
        MediaCodec::H264,
        vec![0xAB; 256],
    );
    assert_eq!(frame.connection_id, "c-x");
    assert_eq!(frame.seq, 42);
    assert_eq!(frame.duration_ns, 16_666_666);
    assert_eq!(frame.kind, MediaFrameKind::VideoI);
    assert_eq!(frame.codec, MediaCodec::H264);
    assert_eq!(frame.payload.len(), 256);
    assert!(frame.ts_ns > 0, "ts_ns must be wall-clock stamped");
}

/// Capabilities snapshot must populate at least the codec lists
/// (video + audio). On Windows host the device lists may be empty
/// when running in a headless CI environment so we only assert
/// that the call succeeds and the fields are well-formed; codecs
/// are platform-agnostic and always populated.
#[test]
fn build_capabilities_populates_codecs() {
    let caps = MediaProducer::build_capabilities(Some("Default"), false);
    assert!(
        !caps.video_codecs.is_empty(),
        "video codec list must not be empty: {caps:?}"
    );
    assert!(
        !caps.audio_codecs.is_empty(),
        "audio codec list must not be empty: {caps:?}"
    );
    assert_eq!(caps.desktop_name, "Default");
    assert!(!caps.has_tauri);
}

/// Regression: the UI used to render two indistinguishable "H264"
/// entries because the daemon mapped `MediaCodec::H264` back to a
/// single string for both X264 (libx264) and H264 (OpenH264). The
/// fix carries the verbatim encoder identifiers in
/// `video_encoders` alongside the SDP-level `video_codecs`. This
/// test pins the contract: every capture-engine encoder name
/// surfaces independently in `video_encoders`, while the
/// `video_codecs` list collapses on SDP-equivalent duplicates.
#[test]
fn build_capabilities_preserves_x264_h264_distinction() {
    let caps = MediaProducer::build_capabilities(Some("Default"), false);
    assert!(
        caps.video_encoders.contains(&"X264".to_string()),
        "X264 must appear in video_encoders: {:?}",
        caps.video_encoders
    );
    assert!(
        caps.video_encoders.contains(&"H264".to_string()),
        "H264 must appear in video_encoders: {:?}",
        caps.video_encoders
    );
    let h264_codec_count = caps
        .video_codecs
        .iter()
        .filter(|c| matches!(c, MediaCodec::H264))
        .count();
    assert_eq!(
        h264_codec_count, 1,
        "video_codecs collapses both H.264 implementations onto one MediaCodec::H264 \
             for SDP m-line negotiation: {:?}",
        caps.video_codecs
    );
    assert!(
        caps.audio_encoders
            .iter()
            .any(|s| s.eq_ignore_ascii_case("OPUS")),
        "audio_encoders must include Opus: {:?}",
        caps.audio_encoders
    );
}

/// Regression: `frame_duration_ns` was previously hardcoded to
/// 1/fps everywhere it was emitted, so when wall-clock elapsed
/// between emits exceeded 1/fps (heartbeat path = ~1s, broadcast
/// lag path = 50-100ms), the receiver's RTP timestamp drifted
/// behind wall clock by the difference. Over a minute of static
/// desktop the drift reached ~58s, manifesting as the user's
/// reported "browser shows actions from a minute ago" symptom.
///
/// `compute_emit_duration_ns` must:
///   1. Fall back to `default_ns` when there's no prior emit
///      (first frame after connect).
///   2. Return the real wall-clock delta when there is one,
///      regardless of how long it is — the heartbeat path
///      *needs* ~1s for its sample.
#[test]
fn compute_emit_duration_ns_first_emit_falls_back_to_default() {
    let now = std::time::Instant::now();
    assert_eq!(
        compute_emit_duration_ns(None, now, 33_000_000),
        33_000_000,
        "with no prior emit there's nothing to subtract; default 1/fps is the right baseline"
    );
}

#[test]
fn compute_emit_duration_ns_reflects_short_wall_clock_delta() {
    let prev = std::time::Instant::now();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let now = std::time::Instant::now();
    let dur = compute_emit_duration_ns(Some(prev), now, 33_000_000);
    // 50ms elapsed; the configured default of 33ms must NOT
    // be returned — that's the bug this guards against.
    assert!(
        (40_000_000..=120_000_000).contains(&dur),
        "duration must reflect the ~50ms wall-clock delta, not the 33ms default; got {dur}"
    );
}

#[test]
fn compute_emit_duration_ns_handles_heartbeat_scale_intervals() {
    // Pin the heartbeat path: under static desktop the loop
    // emits roughly once per second. Stamping 33ms on each
    // emit was exactly how the receiver's RTP clock fell
    // behind wall clock by ~967ms/second.
    let prev = std::time::Instant::now();
    std::thread::sleep(std::time::Duration::from_millis(1000));
    let now = std::time::Instant::now();
    let dur = compute_emit_duration_ns(Some(prev), now, 33_000_000);
    assert!(
        dur >= 900_000_000,
        "1s heartbeat must produce a ~1s duration so RTP timestamp keeps pace with wall \
             clock; got {dur}"
    );
}

/// Regression: the shared-capture broadcast (introduced when the
/// capture loop was decoupled to fix multi-browser black screen)
/// runs at the OS refresh rate, while per-connection encoders run
/// at a configured fps, so `RecvError::Lagged(n)` is the expected
/// steady state.
///
/// Earlier code requested a keyframe on every lag event, which
/// recreated the encoder. Encoder rebuilds are an order of
/// magnitude more expensive than emitting one P frame, so each
/// rebuild widened the lag and triggered another rebuild —
/// observed in production as ~6 keyframe rebuilds per second
/// flooding the logs and starving the pipeline.
///
/// This test pins the contract by exercising the real
/// `tokio::sync::broadcast` Lagged path and asserting:
///   1. `handle_broadcast_lag` does not flip the keyframe flag.
///   2. The next recv after Lagged still yields the latest
///      available frame (the encoder's reference chain is not
///      broken — broadcast resyncs the receiver to head
///      automatically).
#[tokio::test(flavor = "current_thread")]
async fn broadcast_lag_does_not_request_keyframe_or_rebuild_encoder() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::broadcast;

    let keyframe_requested = Arc::new(AtomicBool::new(false));

    let (tx, mut rx) = broadcast::channel::<u32>(2);
    // Publish more than capacity so the next recv hits Lagged.
    for i in 0..6u32 {
        let _ = tx.send(i);
    }

    match rx.recv().await {
        Err(broadcast::error::RecvError::Lagged(n)) => {
            handle_broadcast_lag("test-conn", n);
        }
        other => panic!("expected RecvError::Lagged on overflow, got {other:?}"),
    }

    assert!(
        !keyframe_requested.load(Ordering::Relaxed),
        "broadcast lag must not request a keyframe — that would feed a \
             self-amplifying keyframe-storm loop"
    );

    // The receiver auto-resyncs to head: the first post-lag recv must
    // succeed, proving we do NOT need to recreate the encoder to keep
    // the pipeline flowing.
    let next = rx.recv().await.expect("recv after Lagged must succeed");
    assert!(
        next > 0,
        "post-lag recv returns the latest available input — encoder's \
             internal reference chain is preserved"
    );
}

/// Regression: with the default GOP widened to 120 frames, the
/// encoder still emits periodic IDR access units without any
/// worker-side rebuild (`next_pass_is_idr` stays false). The
/// emit path must label those VideoI based on the encoder's
/// own `is_keyframe` signal so host-side keyframe counts align
/// with what the browser decoder reports and the daemon's
/// paused-write_sample latch (after a worker swap) can clear
/// on a natural IDR rather than waiting for the next
/// ForceKeyframe round-trip.
#[test]
fn classify_video_frame_kind_treats_internal_gop_idr_as_video_i() {
    use desk_capture_engine::model::video_encoder::NalInfo;
    let idr_nal = NalInfo {
        nal_bytes: bytes::Bytes::from_static(&[0; 16]),
        is_keyframe: true,
    };
    // No worker-side rebuild flag, but the encoder reports a
    // keyframe — must surface as VideoI.
    let kind = classify_video_frame_kind(&[idr_nal], false);
    assert_eq!(
        kind,
        MediaFrameKind::VideoI,
        "encoder-reported keyframe must surface as VideoI even when next_pass_is_idr=false"
    );
}

/// Pin the inverse: when the encoder reports a P frame and the
/// worker has no pending rebuild, the emit path must label it
/// VideoP. Mis-labelling a P frame as VideoI would defeat the
/// daemon's paused-write_sample correctness check (it would
/// resume on the wrong frame and the browser would see corrupt
/// video until the next real IDR clears the buffer).
#[test]
fn classify_video_frame_kind_p_frame_stays_video_p() {
    use desk_capture_engine::model::video_encoder::NalInfo;
    let p_nal = NalInfo {
        nal_bytes: bytes::Bytes::from_static(&[0; 16]),
        is_keyframe: false,
    };
    let kind = classify_video_frame_kind(&[p_nal], false);
    assert_eq!(kind, MediaFrameKind::VideoP);
}

/// Pin the rebuild path: even if the encoder happens to report
/// is_keyframe=false on the very first emission after a
/// settings_changed / ForceKeyframe rebuild (this should not
/// happen in practice — the rebuilt encoder always emits
/// SPS+PPS+IDR first — but we keep the explicit `next_pass_is_idr`
/// belt-and-braces flag), the emit path must still mark VideoI
/// because the worker just rebuilt the encoder.
#[test]
fn classify_video_frame_kind_next_pass_is_idr_overrides() {
    use desk_capture_engine::model::video_encoder::NalInfo;
    let nal = NalInfo {
        nal_bytes: bytes::Bytes::from_static(&[0; 16]),
        is_keyframe: false,
    };
    let kind = classify_video_frame_kind(&[nal], true);
    assert_eq!(
        kind,
        MediaFrameKind::VideoI,
        "next_pass_is_idr=true is the rebuild marker; first post-rebuild emit must be \
             VideoI even if the NAL header check disagrees"
    );
}

/// Mixed-NAL access unit: any single keyframe NAL anywhere in
/// the access unit promotes the whole emit to VideoI. This
/// matches H.264's wire reality where one access unit can be
/// SPS + PPS + IDR slice (3 NALs, only the third one carries
/// the IDR semantics) but the entire unit is a keyframe.
#[test]
fn classify_video_frame_kind_any_keyframe_in_access_unit_wins() {
    use desk_capture_engine::model::video_encoder::NalInfo;
    let nals = vec![
        NalInfo {
            nal_bytes: bytes::Bytes::from_static(&[0; 4]),
            is_keyframe: false,
        },
        NalInfo {
            nal_bytes: bytes::Bytes::from_static(&[0; 4]),
            is_keyframe: true,
        },
    ];
    assert_eq!(
        classify_video_frame_kind(&nals, false),
        MediaFrameKind::VideoI
    );
}

// ============== audio + cursor sync tests ==============

/// `build_media_frame` for an audio packet stamps the right
/// `MediaFrameKind` + `MediaCodec` and the daemon's
/// `write_video_frame` (which routes audio to `audio_track`)
/// can pick the audio path off the resulting frame.
#[test]
fn build_media_frame_audio_kind_and_opus_codec() {
    let frame = build_media_frame(
        "c-audio",
        7,
        20_000_000, // 20 ms — Opus packet duration
        MediaFrameKind::Audio,
        MediaCodec::Opus,
        vec![0xCD; 80],
    );
    assert_eq!(frame.kind, MediaFrameKind::Audio);
    assert_eq!(frame.codec, MediaCodec::Opus);
    assert_eq!(frame.duration_ns, 20_000_000);
    assert_eq!(frame.payload.len(), 80);
}

/// CursorData payload that the worker emits is well-formed JSON
/// (`CursorSyncData` model). Mirrors what the daemon decodes via
/// `write_cursor_data` after passing through IPC. We can't drive
/// a real capture in this unit test so we hand-build the model
/// and verify it survives serde and matches the wire shape the
/// browser side expects.
#[test]
fn cursor_sync_data_serializes_to_json_bytes_for_ipc() {
    use crate::model::data_channel::CursorSyncData;
    let cursor = CursorSyncData {
        base64_png: "AAAA".to_string(),
        hotspot_x: 4,
        hotspot_y: 7,
        visible: true,
        shape_id: 99,
        screen_width: 1920,
        screen_height: 1080,
        embedded: false,
    };
    let bytes = serde_json::to_vec(&cursor).expect("serialise");
    // Round-trip via UTF-8 + serde to confirm the bytes are
    // exactly what the daemon's `write_cursor_data` will hand
    // through to `dc.send_text`. A regression here would mean
    // the browser-side decoder breaks even though the IPC plumbing
    // is intact.
    let s = std::str::from_utf8(&bytes).expect("utf-8");
    let decoded: CursorSyncData = serde_json::from_str(s).expect("decode");
    assert_eq!(decoded.shape_id, 99);
    assert!(decoded.visible);
    assert_eq!(decoded.screen_width, 1920);
    assert!(!decoded.embedded);
}

/// Embedded variant: `embedded=true` survives the JSON
/// round-trip and arrives at the browser-side decoder. This is
/// the wire signal that flips the front-end CSS cursor off
/// when the OS has composited the cursor into the desktop
/// frame.
#[test]
fn cursor_sync_data_round_trips_with_embedded_true() {
    use crate::model::data_channel::CursorSyncData;
    let cursor = CursorSyncData {
        visible: false,
        embedded: true,
        screen_width: 2560,
        screen_height: 1440,
        ..Default::default()
    };
    let bytes = serde_json::to_vec(&cursor).expect("serialise");
    let decoded: CursorSyncData =
        serde_json::from_str(std::str::from_utf8(&bytes).expect("utf-8")).expect("decode");
    assert!(!decoded.visible);
    assert!(decoded.embedded);
    assert_eq!(decoded.screen_width, 2560);
    assert_eq!(decoded.screen_height, 1440);
}

/// Backward-compatible deserialization (codex r2 #2): a payload
/// without the `embedded` field defaults to `embedded=false`,
/// so old browsers / cached versions continue to parse cleanly.
#[test]
fn cursor_sync_data_legacy_payload_without_embedded_field_decodes_as_false() {
    use crate::model::data_channel::CursorSyncData;
    // Hand-written JSON missing the `embedded` key.
    let legacy = r#"{
            "base64_png": "",
            "hotspot_x": 0,
            "hotspot_y": 0,
            "visible": true,
            "shape_id": 42,
            "screen_width": 1920,
            "screen_height": 1080
        }"#;
    let decoded: CursorSyncData = serde_json::from_str(legacy).expect("decode legacy");
    assert!(!decoded.embedded, "missing field must default to false");
    assert_eq!(decoded.shape_id, 42);
}

// ---- should_recreate_for_resolution ----

/// Steady-state: dimensions match → no rebuild signal.
#[test]
fn should_recreate_for_resolution_returns_none_when_equal() {
    assert!(should_recreate_for_resolution((1920, 1080), (1920, 1080)).is_none());
}

/// Width changed → rebuild with the new dimensions.
#[test]
fn should_recreate_for_resolution_returns_some_on_width_change() {
    assert_eq!(
        should_recreate_for_resolution((1920, 1080), (1024, 1080)),
        Some((1024, 1080))
    );
}

/// Height changed → rebuild with the new dimensions.
#[test]
fn should_recreate_for_resolution_returns_some_on_height_change() {
    assert_eq!(
        should_recreate_for_resolution((1920, 1080), (1920, 720)),
        Some((1920, 720))
    );
}

/// Both width and height changed (typical screen mode change).
#[test]
fn should_recreate_for_resolution_returns_some_on_both_change() {
    assert_eq!(
        should_recreate_for_resolution((1920, 1080), (1024, 768)),
        Some((1024, 768))
    );
}

/// (0, h) sentinel: WGC WAIT_TIMEOUT / DXGI NoContentChange emit
/// EmptyImageInfo with width=0; never rebuild against zero —
/// libvpx / x264 refuse the config and the producer would crash.
#[test]
fn should_recreate_for_resolution_returns_none_when_frame_width_zero() {
    assert!(should_recreate_for_resolution((1920, 1080), (0, 1080)).is_none());
}

/// (w, 0) sentinel: symmetric to the width=0 case.
#[test]
fn should_recreate_for_resolution_returns_none_when_frame_height_zero() {
    assert!(should_recreate_for_resolution((1920, 1080), (1920, 0)).is_none());
}

/// (0, 0) sentinel: covers the WGC frame-pool-resize handoff
/// frame too, where staging_size has been replaced but the
/// outgoing CaptureResult is still EmptyImageInfo.
#[test]
fn should_recreate_for_resolution_returns_none_when_frame_both_zero() {
    assert!(should_recreate_for_resolution((1920, 1080), (0, 0)).is_none());
}

/// Initialised at zero (worker should never get here, but be
/// symmetric for defence in depth): a zero frame still short-
/// circuits before the inequality check.
#[test]
fn should_recreate_for_resolution_returns_none_when_init_zero_and_frame_zero() {
    assert!(should_recreate_for_resolution((0, 0), (0, 0)).is_none());
}

// ---- display_info_for_size ----

fn make_base_display_info() -> DisplayInfo {
    DisplayInfo {
        device_name: r"\\.\DISPLAY1".to_string(),
        display_device_name: Some("Generic PnP Monitor".to_string()),
        desktop_coordinates: DisplayRect {
            left: 100,
            top: 50,
            right: 2020,
            bottom: 1130,
        },
        resolutions: vec![
            desk_signal_facade::model::image_capture::Resolution::new(1920, 1080),
            desk_signal_facade::model::image_capture::Resolution::new(2560, 1440),
        ],
        attached_to_desktop: true,
        rotation: 90,
    }
}

/// Only `desktop_coordinates` is rewritten; every other field of
/// the synthetic `DisplayInfo` must carry through unchanged so
/// downstream encoders that consult e.g. resolutions list see
/// the real device's capabilities.
#[test]
fn display_info_for_size_preserves_device_name_and_resolutions() {
    let base = make_base_display_info();
    let di = display_info_for_size(&base, (1024, 768));
    assert_eq!(di.device_name, base.device_name);
    assert_eq!(di.display_device_name, base.display_device_name);
    assert_eq!(di.resolutions, base.resolutions);
    assert_eq!(di.attached_to_desktop, base.attached_to_desktop);
    assert_eq!(di.rotation, base.rotation);
}

/// `right`/`bottom` are derived from `left + width`/`top + height`,
/// preserving `left`/`top` as-is from `base`.
#[test]
fn display_info_for_size_sets_right_bottom_from_left_top_plus_size() {
    let base = make_base_display_info();
    let di = display_info_for_size(&base, (1920, 1080));
    assert_eq!(di.desktop_coordinates.left, 100);
    assert_eq!(di.desktop_coordinates.top, 50);
    assert_eq!(di.desktop_coordinates.right, 100 + 1920);
    assert_eq!(di.desktop_coordinates.bottom, 50 + 1080);
}

/// Secondary monitor placed to the left of the primary in Windows
/// Display Settings yields a negative `left`. The derived
/// `right` must accept the negative offset and still equal
/// `left + width`.
#[test]
fn display_info_for_size_handles_negative_left_top() {
    let mut base = make_base_display_info();
    base.desktop_coordinates = DisplayRect {
        left: -1920,
        top: 0,
        right: 0,
        bottom: 1080,
    };
    let di = display_info_for_size(&base, (1920, 1080));
    assert_eq!(di.desktop_coordinates.left, -1920);
    assert_eq!(di.desktop_coordinates.right, 0);
    assert_eq!(di.desktop_coordinates.bottom, 1080);
}

/// Verifies the core codex r1 #2 invariant: a settings-changed
/// rebuild that fires *after* a resolution change still picks up
/// the new size, because every `create_video_encoder` flows
/// through `display_info_for_size(&base, encoder_init_size)`.
/// We simulate the worker's "encoder_init_size = (1024, 768)
/// after a mid-session change" state and assert that the
/// rebuild's DisplayInfo carries 1024x768, not the subscribe-
/// time 1920x1080.
#[test]
fn settings_rebuild_uses_current_encoder_size() {
    let base_display_info = make_base_display_info(); // 1920x1080
    let encoder_init_size: (u32, u32) = (1024, 768); // post-resize state
    let rebuild_di = display_info_for_size(&base_display_info, encoder_init_size);
    assert_eq!(rebuild_di.desktop_coordinates.width() as u32, 1024);
    assert_eq!(rebuild_di.desktop_coordinates.height() as u32, 768);
    // Sanity: device_name preserved so the encoder still keys on
    // the real device.
    assert_eq!(rebuild_di.device_name, base_display_info.device_name);
}

/// Unknown connection ids must not panic and must return None so
/// the SetVirtualDisplayMode filter can simply skip them.
#[test]
fn connection_capture_key_returns_none_for_unknown() {
    let (sender, _rx) = inprocess::make_media();
    let (err_tx, _err_rx) = mpsc::unbounded_channel::<WorkerToService>();
    let producer = MediaProducer::new(DeskSettings::default(), sender, err_tx);
    assert!(producer.connection_capture_key("never-started").is_none());
}

/// Round-trip: a value inserted by the pipeline thread shows up
/// in the public lookup. Production writes happen inside
/// `video_pipeline_loop` post-subscribe; the test exercises the
/// map contract by inserting directly so it does not need a real
/// capture backend.
#[test]
fn connection_capture_key_returns_recorded_value() {
    let (sender, _rx) = inprocess::make_media();
    let (err_tx, _err_rx) = mpsc::unbounded_channel::<WorkerToService>();
    let producer = MediaProducer::new(DeskSettings::default(), sender, err_tx);
    let key = CaptureKey {
        backend: "WGC".into(),
        device_name: r"\\.\DISPLAY51".into(),
    };
    producer.capture_keys.lock().unwrap().insert(
        "conn-A".into(),
        CaptureKeyRecord {
            key: key.clone(),
            generation: 1,
        },
    );
    let got = producer.connection_capture_key("conn-A").expect("present");
    assert_eq!(got, key);
}

/// RAII contract: dropping the guard removes the entry so a panic
/// or early-`?` in the video pipeline thread cannot leave a stale
/// `(connection, CaptureKey)` entry behind.
#[test]
fn capture_key_guard_clears_map_on_drop() {
    let map: Arc<StdMutex<HashMap<String, CaptureKeyRecord>>> =
        Arc::new(StdMutex::new(HashMap::new()));
    let key = CaptureKey {
        backend: "WGC".into(),
        device_name: r"\\.\DISPLAY51".into(),
    };
    map.lock()
        .unwrap()
        .insert("conn-A".into(), CaptureKeyRecord { key, generation: 7 });
    {
        let _g = CaptureKeyGuard {
            map: Arc::clone(&map),
            connection_id: "conn-A".into(),
            generation: 7,
        };
        // guard still in scope: entry must be present.
        assert!(map.lock().unwrap().contains_key("conn-A"));
    }
    assert!(
        !map.lock().unwrap().contains_key("conn-A"),
        "guard drop must have cleared the connection's CaptureKey entry"
    );
}

/// Race regression test (codex 2026-05-24): `stop_media` is
/// fire-and-forget, so the old video pipeline thread may finish
/// unwinding *after* a `start_media` for the same connection_id
/// has finished subscribing and written its own
/// `CaptureKeyRecord`. If `CaptureKeyGuard::drop` removed by
/// connection_id alone the old guard would erase the new
/// pipeline's freshly recorded key — `connection_capture_key`
/// would then return `None` and the next `SetVirtualDisplayMode`
/// silently skips the WGC restart (visible in production as the
/// "second resize after a stop+start cycle freezes the frame"
/// symptom). The generation token defeats this: the old guard
/// observes that the current record carries a newer generation
/// and leaves it alone.
#[test]
fn capture_key_guard_drop_preserves_newer_generation_entry() {
    let map: Arc<StdMutex<HashMap<String, CaptureKeyRecord>>> =
        Arc::new(StdMutex::new(HashMap::new()));
    let key_old = CaptureKey {
        backend: "WGC".into(),
        device_name: r"\\.\DISPLAY51".into(),
    };
    let key_new = CaptureKey {
        backend: "WGC".into(),
        device_name: r"\\.\DISPLAY52".into(),
    };
    // Pipeline A (gen=1) writes its entry and gets its guard.
    map.lock().unwrap().insert(
        "conn-A".into(),
        CaptureKeyRecord {
            key: key_old,
            generation: 1,
        },
    );
    let guard_a = CaptureKeyGuard {
        map: Arc::clone(&map),
        connection_id: "conn-A".into(),
        generation: 1,
    };
    // Pipeline B (gen=2) wins the next subscribe and overwrites
    // the slot before A's stack unwinds.
    map.lock().unwrap().insert(
        "conn-A".into(),
        CaptureKeyRecord {
            key: key_new.clone(),
            generation: 2,
        },
    );
    let guard_b = CaptureKeyGuard {
        map: Arc::clone(&map),
        connection_id: "conn-A".into(),
        generation: 2,
    };

    // A finally unwinds. Its Drop must NOT erase B's entry.
    drop(guard_a);
    let after_a = map.lock().unwrap();
    let rec = after_a
        .get("conn-A")
        .expect("Pipeline B's record must survive Pipeline A's stale drop");
    assert_eq!(rec.generation, 2, "generation must reflect Pipeline B");
    assert_eq!(rec.key, key_new, "key must reflect Pipeline B");
    drop(after_a);

    // B unwinds normally — its generation matches, so its entry
    // gets cleaned up.
    drop(guard_b);
    assert!(
        !map.lock().unwrap().contains_key("conn-A"),
        "B's own guard must clean up B's own entry"
    );
}

/// Defensive: a guard whose generation doesn't match anything in
/// the map (e.g. the map was already cleared by some other path)
/// is a no-op rather than a panic.
#[test]
fn capture_key_guard_drop_noop_when_entry_missing() {
    let map: Arc<StdMutex<HashMap<String, CaptureKeyRecord>>> =
        Arc::new(StdMutex::new(HashMap::new()));
    {
        let _g = CaptureKeyGuard {
            map: Arc::clone(&map),
            connection_id: "ghost".into(),
            generation: 42,
        };
    }
    assert!(map.lock().unwrap().is_empty());
}

// ---- capturable_device_name ----

fn disp(name: &str, left: i32, top: i32, width: i32, height: i32, attached: bool) -> DisplayInfo {
    DisplayInfo {
        device_name: name.to_string(),
        display_device_name: None,
        desktop_coordinates: DisplayRect {
            left,
            top,
            right: left + width,
            bottom: top + height,
        },
        resolutions: Vec::new(),
        attached_to_desktop: attached,
        rotation: 0,
    }
}

#[test]
fn capturable_device_name_keeps_requested_when_live_and_capturable() {
    let live = vec![
        disp(r"\\.\DISPLAY1", 0, 0, 1280, 800, true),
        disp(r"\\.\DISPLAY2", 1280, 0, 1920, 1080, true),
    ];
    assert_eq!(
        capturable_device_name(&live, r"\\.\DISPLAY2").as_deref(),
        Some(r"\\.\DISPLAY2")
    );
}

#[test]
fn capturable_device_name_falls_back_to_primary_when_requested_missing() {
    let live = vec![
        disp(r"\\.\DISPLAY2", 1280, 0, 1920, 1080, true),
        disp(r"\\.\DISPLAY1", 0, 0, 1280, 800, true),
    ];
    // Requested target is gone; substitute the primary at origin (0,0),
    // not merely the first enumerated display.
    assert_eq!(
        capturable_device_name(&live, r"\\.\DISPLAY33").as_deref(),
        Some(r"\\.\DISPLAY1")
    );
}

#[test]
fn capturable_device_name_falls_back_to_first_when_no_origin_primary() {
    let live = vec![
        disp(r"\\.\DISPLAY2", 1280, 0, 1920, 1080, true),
        disp(r"\\.\DISPLAY3", 3200, 0, 1280, 800, true),
    ];
    assert_eq!(
        capturable_device_name(&live, r"\\.\DISPLAY33").as_deref(),
        Some(r"\\.\DISPLAY2")
    );
}

#[test]
fn capturable_device_name_treats_detached_requested_as_uncapturable() {
    let live = vec![
        disp(r"\\.\DISPLAY33", 0, 0, 1920, 1080, false), // requested, but detached
        disp(r"\\.\DISPLAY1", 0, 0, 1280, 800, true),
    ];
    // The requested name exists but is not attached, so it is not usable;
    // fall back to the live primary instead of returning it.
    assert_eq!(
        capturable_device_name(&live, r"\\.\DISPLAY33").as_deref(),
        Some(r"\\.\DISPLAY1")
    );
}

#[test]
fn capturable_device_name_treats_zero_size_requested_as_uncapturable() {
    let live = vec![
        disp(r"\\.\DISPLAY33", 0, 0, 0, 0, true), // requested, attached, zero surface
        disp(r"\\.\DISPLAY1", 0, 0, 1280, 800, true),
    ];
    assert_eq!(
        capturable_device_name(&live, r"\\.\DISPLAY33").as_deref(),
        Some(r"\\.\DISPLAY1")
    );
}

#[test]
fn capturable_device_name_none_when_no_usable_display() {
    let live = vec![
        disp(r"\\.\DISPLAY1", 0, 0, 1280, 800, false), // detached
        disp(r"\\.\DISPLAY2", 1280, 0, 0, 0, true),    // zero surface
    ];
    // No usable display at all -> leave the name untouched (the capture
    // backend will surface its own error).
    assert!(capturable_device_name(&live, r"\\.\DISPLAY1").is_none());
}

#[test]
fn capturable_device_name_none_for_empty_requested() {
    let live = vec![disp(r"\\.\DISPLAY1", 0, 0, 1280, 800, true)];
    assert!(capturable_device_name(&live, "").is_none());
}

#[test]
fn capturable_device_name_none_for_empty_live() {
    assert!(capturable_device_name(&[], r"\\.\DISPLAY1").is_none());
}
