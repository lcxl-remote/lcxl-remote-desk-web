use super::*;

/// Map a capture-engine codec name string to the IPC `MediaCodec` enum.
/// Returns `None` for codec names the IPC layer does not know about
/// (silently dropped — newer worker workers may add codecs the daemon
/// is not yet compiled against, and we should not crash on that).
///
/// `is_video` selects the video set (X264/H264/VP8/VP9/AV1) vs. the
/// audio set (Opus). The factory list APIs return strings that overlap
/// (e.g. "H264" for video, "Opus" for audio).
pub(super) fn codec_from_str(name: &str, is_video: bool) -> Option<MediaCodec> {
    if is_video {
        match name {
            "H264" | "X264" => Some(MediaCodec::H264),
            "VP8" => Some(MediaCodec::Vp8),
            "VP9" => Some(MediaCodec::Vp9),
            "AV1" => Some(MediaCodec::Av1),
            _ => None,
        }
    } else {
        match name {
            "Opus" => Some(MediaCodec::Opus),
            _ => None,
        }
    }
}

/// Drain every pending `UpdateMediaSettingsPayload` from the per-
/// connection mpsc and apply each to `merged_settings`. The tick + fps
/// path is a `tokio::time::interval`, which the loop replaces on fps
/// Compute the wall-clock duration to attach to the next emitted
/// `MediaFrame`. The daemon hands this straight to webrtc-rs's
/// `Sample.duration`, which advances the RTP timestamp by
/// `duration_secs * 90000Hz` for video. Earlier code passed the
/// configured 1/fps interval as a fixed value, which was wrong for
/// two reasons:
///
///   - **Static-desktop heartbeat path.** Heartbeats fire every
///     ~1s but stamped duration=33ms. Each second of static
///     desktop made the receiver's RTP clock fall ~967ms behind
///     wall clock. After a minute of idle, the browser's
///     playout buffer held nearly a minute of "future" frames —
///     so when the user finally moved the mouse, the browser
///     replayed minutes-old activity instead of showing live
///     events.
///
///   - **Broadcast lag path.** When the encoder loop falls
///     behind the OS-rate capture loop (`RecvError::Lagged`),
///     real wall-clock interval can be 50-100ms; stamping 33ms
///     made the same drift accumulate at a smaller per-event
///     rate.
///
/// The first emit has no `prev_emit` reference, so we fall back
/// to the configured 1/fps default — the receiver's first
/// timestamp doesn't matter for delta calculations.
pub(super) fn compute_emit_duration_ns(
    prev_emit: Option<std::time::Instant>,
    now: std::time::Instant,
    default_ns: u64,
) -> u64 {
    match prev_emit {
        Some(prev) => now.duration_since(prev).as_nanos().min(u64::MAX as u128) as u64,
        None => default_ns,
    }
}

/// Classify the `MediaFrameKind` for an outgoing video access unit.
/// Returns `VideoI` if either:
///   - The worker just rebuilt the encoder (`next_pass_is_idr=true`,
///     covers initial start, settings_changed rebuild, ForceKeyframe
///     rebuild) — the very first encoder output is by construction
///     SPS+PPS+IDR.
///   - Any NAL in this access unit reports `is_keyframe=true` from
///     the encoder's own frame-type signal — covers the periodic
///     internal-GOP IDR that the encoder emits without any worker
///     rebuild (with the default GOP=120 this happens roughly every
///     2 s at 60 fps).
/// Pinned as a helper so the "encoder GOP IDR is also VideoI" contract
/// is unit-testable independently of the surrounding async loop.
#[inline]
pub(super) fn classify_video_frame_kind(
    nals: &[desk_capture_engine::model::video_encoder::NalInfo],
    next_pass_is_idr: bool,
) -> MediaFrameKind {
    let any_keyframe = nals.iter().any(|n| n.is_keyframe);
    if next_pass_is_idr || any_keyframe {
        MediaFrameKind::VideoI
    } else {
        MediaFrameKind::VideoP
    }
}

/// Handler for `RecvError::Lagged(n)` on the shared-capture
/// broadcast subscription. Pinned as a separate function so the
/// "lag does NOT request a keyframe" contract is unit-testable
/// and any future regression that re-introduces an encoder
/// rebuild here gets caught.
///
/// The body intentionally has no side effects on the encoder /
/// keyframe state: see the call site in `video_pipeline_loop`
/// for the reasoning. This logs at DEBUG (not WARN) because lag
/// is the expected steady-state behaviour when capture runs
/// faster than the per-connection fps throttle.
#[inline]
pub(super) fn handle_broadcast_lag(connection_id: &str, n: u64) {
    debug!(
        "[MediaProducer:{connection_id}] shared-capture broadcast lagged by {n} \
         frames; skipping ahead to the latest available input"
    );
}

/// Outcome of draining the live-settings channel on one encode tick.
pub(super) struct SettingsDrainOutcome {
    /// At least one knob that requires an encoder rebuild (fps /
    /// quality) actually changed.
    pub(super) needs_rebuild: bool,
    /// At least one user-facing live video setting was consumed and
    /// applied. This is deliberately broader than `needs_rebuild`:
    /// dirty-rect and cursor visibility take effect without recreating
    /// the encoder, but the daemon still needs a terminal Streaming
    /// acknowledgement to complete the correlated settings request.
    pub(super) live_settings_applied: bool,
    /// Latest bitrate-cap directive in the drained batch, if any:
    /// `Some(Some(k))` caps at `k` kbps, `Some(None)` clears the cap
    /// (wire sentinel `bitrate_kbps == Some(0)`), `None` means the
    /// batch carried no cap directive. Applied to the encoder via
    /// `VideoEncoder::set_bitrate_cap` — never by rebuilding, since
    /// cap updates arrive at REMB cadence (~1 Hz) and a rebuild per
    /// update would cause an IDR storm.
    pub(super) cap_directive: Option<Option<u32>>,
}

/// Drains every pending `UpdateMediaSettingsPayload` and folds the
/// encoder-relevant knobs into `merged_settings`, coalescing a burst
/// of updates into a single outcome. fps changes also retune the
/// frame interval (the ticker can't be adjusted in place).
///
/// `needs_rebuild` is only set when a knob actually changed — we
/// compare to the *current* `merged_settings` rather than the IPC
/// payload directly so coalesced updates that converge to the same
/// value as the live state are no-ops.
pub(super) fn drain_settings_updates(
    connection_id: &str,
    settings_rx: &mut mpsc::UnboundedReceiver<UpdateMediaSettingsPayload>,
    merged_settings: &mut DeskSettings,
    frame_interval: &mut Duration,
    frame_duration_ns: &mut u64,
) -> SettingsDrainOutcome {
    let mut changed = false;
    let mut live_settings_applied = false;
    let mut cap_directive: Option<Option<u32>> = None;
    while let Ok(payload) = settings_rx.try_recv() {
        if let Some(fps) = payload.fps
            && fps > 0
        {
            live_settings_applied = true;
            if fps != merged_settings.video_fps {
                merged_settings.video_fps = fps;
                *frame_interval = merged_settings.get_duration_by_video_fps();
                *frame_duration_ns = frame_interval.as_nanos().min(u64::MAX as u128) as u64;
                changed = true;
            }
        }
        if let Some(q) = payload.quality {
            live_settings_applied = true;
            if q != merged_settings.video_quality {
                merged_settings.video_quality = q;
                changed = true;
            }
        }
        if let Some(enable) = payload.enable_dirty_rect {
            live_settings_applied = true;
            // Live-apply the browser's Advanced-tab kill-switch. We do
            // *not* flip the `changed` flag because the encoder
            // doesn't need to be rebuilt — `merged_settings.
            // enable_dirty_rect` is read per-frame by the encoder via
            // `encode(..., enable_dirty_rect)`, so the next frame
            // picks up the new value without a `create_video_encoder`
            // round-trip.
            if enable != merged_settings.enable_dirty_rect {
                merged_settings.enable_dirty_rect = enable;
            }
        }
        if let Some(show_mouse) = payload.show_mouse {
            live_settings_applied = true;
            merged_settings.show_mouse = show_mouse;
        }
        if let Some(kbps) = payload.bitrate_kbps {
            // Tri-state wire semantics (see the IPC field's doc):
            // Some(0) clears the cap, Some(k>0) caps at k kbps. Keep
            // only the newest directive in the batch — the daemon's
            // controller already rate-limits, and only the latest
            // value matters.
            cap_directive = Some(if kbps == 0 { None } else { Some(kbps) });
            debug!(
                "[MediaProducer:{connection_id}] UpdateMediaSettings.bitrate_kbps={kbps} → cap \
                 directive {:?}",
                cap_directive
            );
        }
    }
    SettingsDrainOutcome {
        needs_rebuild: changed,
        live_settings_applied,
        cap_directive,
    }
}

/// Re-applies the connection's current bitrate cap onto a freshly
/// rebuilt encoder (rebuilds reset codec state, dropping any cap that
/// was applied at runtime). No-op when no cap is active.
pub(super) fn replay_bitrate_cap(
    encoder: &mut Box<dyn VideoEncoder>,
    current_cap_kbps: Option<u32>,
    connection_id: &str,
) {
    if let Some(kbps) = current_cap_kbps
        && !encoder.set_bitrate_cap(Some(kbps))
    {
        debug!(
            "[MediaProducer:{connection_id}] encoder does not support bitrate caps; {kbps} kbps \
             cap not re-applied after rebuild"
        );
    }
}

/// Build a `desk_settings` clone with the per-connection overrides
/// from `StartMediaPayload` baked in (codec, fps). Quality / bitrate
/// honour the connection request when non-zero; zero means "use
/// encoder default" per the IPC docstring.
pub(super) fn payload_overrides(base: &DeskSettings, payload: &StartMediaPayload) -> DeskSettings {
    let mut s = base.clone();
    s.video_encoder = Some(payload.video_encoder.setting_name().to_string());
    if payload.fps > 0 {
        s.video_fps = payload.fps;
    }
    // Per-connection backend choice: when the daemon thread-throughs
    // a value (typically the per-connection `desk_settings.image_capture`
    // from the SDP offer), it overrides the worker's startup snapshot.
    // Without this override the worker would always see `base.image_capture`
    // and a second browser could not pick a different backend than the
    // first — see the IPC field's doc comment for the failure mode.
    s.image_capture = Some(payload.image_capture.clone());
    // Per-connection dirty-rect kill-switch — when the daemon sniffed
    // the value out of the SDP offer's `desk_settings`, honour it
    s.enable_dirty_rect = payload.enable_dirty_rect;
    s.show_mouse = payload.show_mouse;
    // v4 capture-selection fix: the IPC field carries the exact
    // `\\.\DISPLAYn` string the browser selected from the dropdown
    // (sourced via daemon → `StartMediaPayload.video_device`).
    // Overriding the worker's base setting here lets a second browser
    // pick a different monitor than the first without colliding on
    // shared capture state. `None` is reserved for a connection with no
    // video pipeline; remote-desktop offers provide a concrete display.
    if let Some(name) = payload.video_device.as_deref() {
        s.video_device_name = name.to_string();
    }
    s
}

/// `Some(new)` if the encoder must be torn down and rebuilt because
/// the frame dimensions diverge from what the encoder was constructed
/// with. `None` if dimensions match or the frame is the no-content
/// sentinel (width=0 or height=0).
///
/// The (0,0) short-circuit covers `EmptyImageInfo` placeholders
/// emitted by WGC's `WAIT_TIMEOUT` branch, WGC's frame-pool-resize
/// branch, and DXGI's `NoContentChange` branch — every backend
/// surfaces width=0,height=0 for "no real frame this tick", and
/// re-creating the encoder against 0x0 would either error or feed an
/// invalid configuration to libvpx / x264.
pub(super) fn should_recreate_for_resolution(
    init: (u32, u32),
    frame: (u32, u32),
) -> Option<(u32, u32)> {
    if frame.0 == 0 || frame.1 == 0 {
        return None;
    }
    if init != frame { Some(frame) } else { None }
}

/// Resolve the encoder's initial width/height from a geometry snapshot,
/// rejecting every degenerate value on the way.
///
/// The capture backend's `current_capture_resolution` is authoritative
/// when present, with the desktop rectangle as fallback (Windows
/// enumeration leaves the former `None` — see
/// `capture-engine`'s `monitors.rs` — so the rectangle is the normal
/// path there). `None` means "the source size is not known yet"; the
/// caller must wait for a real frame rather than hand a zero to the
/// encoder. Mirrors `should_recreate_for_resolution`'s zero guard on the
/// mid-stream path — the startup path used to lack it, which is how a
/// 0x0 geometry snapshot reached `create_video_encoder`.
pub(super) fn initial_encoder_size(display_info: &DisplayInfo) -> Option<(u32, u32)> {
    let from_capture = display_info
        .current_capture_resolution
        .map(|resolution| (resolution.width, resolution.height))
        .filter(|(width, height)| *width > 0 && *height > 0);
    from_capture.or_else(|| {
        let width = display_info.desktop_coordinates.width();
        let height = display_info.desktop_coordinates.height();
        (width > 0 && height > 0).then_some((width as u32, height as u32))
    })
}

/// Build a synthetic `DisplayInfo` carrying `(width, height)` but
/// preserving every other field from `base` (device_name, resolutions,
/// rotation, attached_to_desktop, display_device_name). Used to feed
/// `create_video_encoder` the *current* encoder size; the encoder only
/// consumes `desktop_coordinates.width()/height()`, so left/top stay
/// as in `base` and right/bottom are derived.
pub(super) fn display_info_for_size(base: &DisplayInfo, size: (u32, u32)) -> DisplayInfo {
    let mut di = base.clone();
    let left = di.desktop_coordinates.left;
    let top = di.desktop_coordinates.top;
    di.desktop_coordinates = DisplayRect {
        left,
        top,
        right: left + size.0 as i32,
        bottom: top + size.1 as i32,
    };
    di.current_capture_resolution = Some(
        desk_signal_facade::model::image_capture::Resolution::new(size.0, size.1),
    );
    di
}

/// Returns a live, capturable device name for `requested` given `live` (the
/// display list for the effective capture backend): the requested name if it
/// is attached and capturable, otherwise the primary (origin 0,0), otherwise
/// the first usable display. Only `attached_to_desktop` displays with a
/// non-zero surface are considered — mirroring the input dispatcher
/// (`enumerate_attached_displays` + `geometry_for_device_in`). Returns `None`
/// when no substitution should happen: empty `requested` (preserve the
/// downstream "no display selected" hard error) or no usable display (leave
/// the name untouched and let the capture backend surface its own error).
pub(super) fn capturable_device_name(live: &[DisplayInfo], requested: &str) -> Option<String> {
    if requested.is_empty() {
        return None;
    }
    let usable: Vec<&DisplayInfo> = live
        .iter()
        .filter(|d| {
            d.attached_to_desktop
                && d.desktop_coordinates.width() > 0
                && d.desktop_coordinates.height() > 0
        })
        .collect();
    if usable.is_empty() {
        return None;
    }
    if usable.iter().any(|d| d.device_name == requested) {
        return Some(requested.to_string());
    }
    let primary = usable
        .iter()
        .find(|d| d.desktop_coordinates.left == 0 && d.desktop_coordinates.top == 0);
    Some(primary.unwrap_or(&usable[0]).device_name.clone())
}

/// Inverse of [`codec_from_str`] for the video subset.
pub(super) fn video_codec_name(c: MediaCodec) -> Option<&'static str> {
    match c {
        MediaCodec::H264 => Some("H264"),
        MediaCodec::Vp8 => Some("VP8"),
        MediaCodec::Vp9 => Some("VP9"),
        MediaCodec::Av1 => Some("AV1"),
        MediaCodec::Opus => None,
    }
}
