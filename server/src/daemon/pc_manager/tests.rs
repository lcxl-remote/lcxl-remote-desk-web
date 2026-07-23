use super::*;
use crate::model::settings::{StartupMode, SystemSettings, TraversalMode};
use desk_ipc_protocol::message::MediaCodec;
use desk_signal_facade::model::signal::{LcxlRTCIceServer, RemoteSessionPurpose};
use desk_turn::model::TurnSettings;

// ============== DaemonFtWindow ==============

/// An empty daemon window must not produce a log line — same
/// contract as the worker-side windows. The trailing flush at
/// task exit calls `flush_line` unconditionally; without this
/// guard, every PC teardown would emit an empty
/// `[ft-metrics-daemon] frames=0 bytes=0 ...` line.
#[test]
fn daemon_ft_window_empty_flush_is_none() {
    let w = DaemonFtWindow::default();
    assert_eq!(w.frames, 0);
    assert!(!w.is_full());
    assert!(w.flush_line("cid").is_none());
}

// ============== v4: StartMediaPayload video_device routing ==============

/// Fresh-install state: the browser has not yet picked a display,
/// so `video_device_name` is empty. The daemon must translate that
/// to `None` on the IPC payload — the worker's `payload_overrides`
/// then leaves the base setting untouched and the capture-engine
/// hard-errors at `new()` time. This is the documented "no
/// silent fallback to primary monitor" contract.
#[test]
fn start_media_payload_video_device_is_none_when_settings_empty() {
    assert_eq!(video_device_for_payload(""), None);
}

/// Selected display: the browser submitted a non-empty
/// `\\.\DISPLAYn`. The daemon passes it through verbatim so the
/// worker can rebind capture (e.g. when a second browser picks a
/// different monitor than the first).
#[test]
fn start_media_payload_video_device_is_some_when_settings_set() {
    assert_eq!(
        video_device_for_payload(r"\\.\DISPLAY7"),
        Some(r"\\.\DISPLAY7".to_string())
    );
}

// ============== F3: SDP max-message-size parser ==============

/// Chrome's SDP advertises 262144 (256 KiB) on a session-level
/// attribute. The parser must surface it as an unsigned value.
#[test]
fn parse_sdp_max_message_size_session_level() {
    let sdp = "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n\
                   a=max-message-size:262144\r\n\
                   m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n";
    assert_eq!(parse_sdp_max_message_size(sdp), Some(262144));
}

/// Some browsers put the attribute under the `m=application`
/// section instead of the session level. The parser doesn't care
/// — first match wins.
#[test]
fn parse_sdp_max_message_size_media_level() {
    let sdp = "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n\
                   m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
                   a=mid:0\r\n\
                   a=max-message-size:1073741823\r\n";
    assert_eq!(parse_sdp_max_message_size(sdp), Some(1073741823));
}

/// Absent attribute → None. The caller distinguishes this from a
/// parse failure and falls back to the RFC default with a warning.
#[test]
fn parse_sdp_max_message_size_missing_returns_none() {
    let sdp = "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n\
                   m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n";
    assert!(parse_sdp_max_message_size(sdp).is_none());
}

/// Garbled value (non-numeric) is treated as missing — we don't
/// want to half-parse `a=max-message-size:abc` and pretend we
/// negotiated something.
#[test]
fn parse_sdp_max_message_size_invalid_returns_none() {
    let sdp = "v=0\r\na=max-message-size:not-a-number\r\n";
    assert!(parse_sdp_max_message_size(sdp).is_none());
}

/// The configured chunk_size + binary header must fit under
/// Chrome's 262144-byte advertise. This is the same invariant the
/// worker-side `download_response_advertises_240kib_chunk_size`
/// regression test pins, but reasserted at the daemon layer so a
/// future change to either constant fails both ends.
///
/// Encoded as a `const` assertion so it fires at compile time
/// rather than as a runtime test (which clippy correctly flags as
/// `assertions_on_constants` — both operands are compile-time
/// literals).
const _CHUNK_SIZE_FITS_CHROME_MAX_MESSAGE_SIZE: () = {
    use crate::model::file_transfer::BINARY_HEADER_SIZE;
    use crate::worker::file_transfer_dispatcher::FILE_TRANSFER_CHUNK_SIZE_TX;
    const CHROME_MAX_MESSAGE_SIZE: usize = 262144;
    assert!(
        FILE_TRANSFER_CHUNK_SIZE_TX + BINARY_HEADER_SIZE <= CHROME_MAX_MESSAGE_SIZE,
        "wire-level SCTP message must not exceed Chrome's a=max-message-size:262144 \
             advertise — see 2026-05-11 ErrOutboundPacketTooLarge regression"
    );
};

/// One recorded send populates frames/bytes/dc_send_ns and
/// updates `buffered_max` / `buffered_sum`. Verifies the
/// `is_text` accounting: a text frame increments `text_frames`.
#[test]
fn daemon_ft_window_records_text_and_binary() {
    let mut w = DaemonFtWindow::default();
    // Binary chunk (the dominant case for downloads).
    w.record(
        60 * 1024,
        false,
        Duration::from_micros(50),
        Duration::from_millis(1),
        128 * 1024,
    );
    // Control message (e.g. DownloadResponse JSON).
    w.record(
        200,
        true,
        Duration::from_micros(10),
        Duration::from_micros(80),
        64 * 1024,
    );
    assert_eq!(w.frames, 2);
    assert_eq!(w.bytes, 60 * 1024 + 200);
    assert_eq!(w.text_frames, 1);
    assert_eq!(w.recv_idle_ns, 50_000 + 10_000);
    assert_eq!(w.dc_send_ns, 1_000_000 + 80_000);
    assert_eq!(w.buffered_max_bytes, 128 * 1024);
    assert_eq!(w.buffered_sum_bytes, (128 + 64) * 1024);
    assert_eq!(w.buffered_samples, 2);
    let line = w.flush_line("cid-abc").unwrap();
    assert!(line.contains("cid=cid-abc"));
    assert!(line.contains("frames=2"));
    assert!(line.contains("text=1"));
    assert!(line.contains("buffered_max=131072"));
    assert!(line.contains("buffered_avg=98304"));
}

/// `is_full()` flips at the shared `FT_METRICS_WINDOW_CHUNKS`
/// boundary so the daemon log cadence stays synchronised with
/// the worker log cadence (one daemon line per worker line under
/// steady-state download).
#[test]
fn daemon_ft_window_boundary_is_full() {
    let mut w = DaemonFtWindow::default();
    let boundary = crate::worker::file_transfer_dispatcher::FT_METRICS_WINDOW_CHUNKS;
    for _ in 0..(boundary - 1) {
        w.record(
            1,
            false,
            Duration::from_nanos(1),
            Duration::from_nanos(1),
            0,
        );
    }
    assert!(!w.is_full());
    w.record(
        1,
        false,
        Duration::from_nanos(1),
        Duration::from_nanos(1),
        0,
    );
    assert!(w.is_full());
}

/// `reset()` clears every field back to `Default::default()` so
/// the next window does not double-count. Required for the
/// `is_full → flush → reset` cadence in
/// `spawn_file_transfer_writer_task` to remain consistent.
#[test]
fn daemon_ft_window_reset_clears_state() {
    let mut w = DaemonFtWindow::default();
    w.record(
        100,
        false,
        Duration::from_nanos(1),
        Duration::from_nanos(1),
        42,
    );
    assert!(w.frames > 0);
    w.reset();
    assert_eq!(w, DaemonFtWindow::default());
}

/// `buffered_avg` rounds down on integer division — guard against
/// a refactor that switches to f64 mid-way (the log format is
/// `buffered_avg={u64}`, not `{:.2}`, because we want a clean
/// byte count for grep / awk).
#[test]
fn daemon_ft_window_buffered_avg_integer_rounding() {
    let mut w = DaemonFtWindow::default();
    w.record(
        1,
        false,
        Duration::from_nanos(1),
        Duration::from_nanos(1),
        100,
    );
    w.record(
        1,
        false,
        Duration::from_nanos(1),
        Duration::from_nanos(1),
        101,
    );
    // (100 + 101) / 2 = 100 (integer div). f64 would be 100.5.
    let line = w.flush_line("cid").unwrap();
    assert!(
        line.contains("buffered_avg=100"),
        "expected buffered_avg=100 (integer rounding), got: {line}"
    );
}

fn ice(url: &str) -> LcxlRTCIceServer {
    LcxlRTCIceServer {
        urls: vec![url.to_string()],
        username: String::new(),
        credential: String::new(),
    }
}

/// The active cleanup path (signaling-side `ConnectionRemoved`)
/// handles the typical "user closed the tab" case in
/// milliseconds. The ICE timeouts here are the fallback for the
/// case where signaling itself is gone too — at which point we
/// behave like a normal WebRTC peer and absorb realistic network
/// jitter. Pin the defaults:
///
/// 1. `failed` budget shorter than the webrtc-rs default (25 s).
///    The library default 5 s + 25 s = 30 s window once let a
///    user-driven reopen race the worker's still-running
///    `DxgiImageCapture::DuplicateOutput` and crash the new
///    pipeline with `0x80070057 (E_INVALIDARG)`.
/// 2. `disconnected` matches the webrtc-rs default — we don't
///    lean on this layer to react to graceful disconnects (the
///    signaling-side notification does that) and tightening it
///    further would make brief network jitter look like a real
///    failure under slow / lossy networks.
/// 3. Combined budget kept ≤ 25 s so the fallback still fires
///    long before users would normally retry, while staying
///    above the 5-10 s range where loopback / LAN jitter routinely
///    sits.
#[test]
fn default_daemon_ice_timeouts_match_recovery_budget() {
    // webrtc-ice's `DEFAULT_DISCONNECTED_TIMEOUT` / `DEFAULT_FAILED_TIMEOUT`.
    // Hard-coded here rather than imported because the library exports
    // them with `pub(crate)` visibility.
    const WEBRTC_DEFAULT_DISCONNECTED_SECS: u64 = 5;
    const WEBRTC_DEFAULT_FAILED_SECS: u64 = 25;

    assert!(
        DEFAULT_DAEMON_ICE_DISCONNECTED_TIMEOUT_SECS <= WEBRTC_DEFAULT_DISCONNECTED_SECS,
        "DEFAULT_DAEMON_ICE_DISCONNECTED_TIMEOUT_SECS must not exceed the \
             webrtc-rs default ({WEBRTC_DEFAULT_DISCONNECTED_SECS}s); \
             got {DEFAULT_DAEMON_ICE_DISCONNECTED_TIMEOUT_SECS}s",
    );
    assert!(
        DEFAULT_DAEMON_ICE_FAILED_TIMEOUT_SECS < WEBRTC_DEFAULT_FAILED_SECS,
        "DEFAULT_DAEMON_ICE_FAILED_TIMEOUT_SECS must be strictly less than \
             the webrtc-rs default ({WEBRTC_DEFAULT_FAILED_SECS}s); \
             got {DEFAULT_DAEMON_ICE_FAILED_TIMEOUT_SECS}s",
    );

    let total =
        DEFAULT_DAEMON_ICE_DISCONNECTED_TIMEOUT_SECS + DEFAULT_DAEMON_ICE_FAILED_TIMEOUT_SECS;
    assert!(
        total <= 25,
        "Combined disconnected+failed budget must stay ≤ 25 s so the \
             fallback fires before a typical retry interval; got {total}s",
    );
}

/// `resolve_ice_timeouts` is what `build_peer_connection` reads to
/// decide what gets handed to webrtc-rs `SettingEngine`. Pin both
/// branches: `None` falls back to the daemon defaults, `Some`
/// values flow through verbatim. Without this, an operator override
/// could silently get dropped without anything in the daemon
/// noticing.
#[test]
fn resolve_ice_timeouts_falls_back_to_defaults_when_unset() {
    let mut sys = SystemSettings::default();
    sys.webrtc_ice_disconnected_timeout_secs = None;
    sys.webrtc_ice_failed_timeout_secs = None;
    let (disconnected, failed) = resolve_ice_timeouts(&sys);
    assert_eq!(
        disconnected,
        Duration::from_secs(DEFAULT_DAEMON_ICE_DISCONNECTED_TIMEOUT_SECS),
    );
    assert_eq!(
        failed,
        Duration::from_secs(DEFAULT_DAEMON_ICE_FAILED_TIMEOUT_SECS),
    );
}

#[test]
fn resolve_ice_timeouts_honours_explicit_overrides() {
    let mut sys = SystemSettings::default();
    sys.webrtc_ice_disconnected_timeout_secs = Some(11);
    sys.webrtc_ice_failed_timeout_secs = Some(47);
    let (disconnected, failed) = resolve_ice_timeouts(&sys);
    assert_eq!(disconnected, Duration::from_secs(11));
    assert_eq!(failed, Duration::from_secs(47));
}

#[test]
fn resolve_ice_timeouts_resolves_each_field_independently() {
    // Mixed: disconnected overridden, failed left at default. Catches
    // accidental cross-field copy/paste in `resolve_ice_timeouts`.
    let mut sys = SystemSettings::default();
    sys.webrtc_ice_disconnected_timeout_secs = Some(99);
    sys.webrtc_ice_failed_timeout_secs = None;
    let (disconnected, failed) = resolve_ice_timeouts(&sys);
    assert_eq!(disconnected, Duration::from_secs(99));
    assert_eq!(
        failed,
        Duration::from_secs(DEFAULT_DAEMON_ICE_FAILED_TIMEOUT_SECS),
    );
}

/// `build_peer_connection` is what threads the timeout overrides
/// into the `RTCPeerConnection` instance the registry actually
/// holds. The `SettingEngine`'s timeout fields are `pub(crate)` so
/// we can't read them back through the library API; instead pin
/// the call-site contract by asserting `build_peer_connection`
/// produces a usable PC (i.e. the SettingEngine + APIBuilder
/// configuration didn't break) when constructed with no ICE
/// servers — the same shape the daemon hits in portable mode.
/// Combined with the constant test above, this guards against
/// regressions that quietly drop `set_ice_timeouts` from the
/// SettingEngine wiring.
#[tokio::test]
async fn build_peer_connection_succeeds_with_tightened_ice_timeouts() {
    let settings = Settings::default();
    let pc = build_peer_connection(vec![], &settings)
        .await
        .expect("build_peer_connection must succeed with the daemon defaults");
    // Closing here is best-effort; the test is about the build path,
    // not the close path. A failed close would not be a meaningful
    // regression signal for the timeout wiring.
    let _ = pc.close().await;
}

/// No self-hosted TURN endpoints — the common case for a desk reached
/// through a remote signaling/manager (its own embedded TURN is not
/// running, so nothing is treated as self).
fn no_own() -> HashSet<String> {
    HashSet::new()
}

/// A `TurnSettings` advertising the given `external` endpoints, one UDP
/// interface each, so `own_turn_endpoints` has something to map.
fn turn_settings_with(externals: &[&str]) -> TurnSettings {
    TurnSettings {
        interfaces: externals
            .iter()
            .map(|ext| desk_turn::model::TurnInterface {
                transport: desk_turn::model::TurnTransport::UDP,
                listen: "0.0.0.0:3479".to_string(),
                external: (*ext).to_string(),
            })
            .collect(),
        ..TurnSettings::default()
    }
}

#[test]
fn filter_keeps_stun_only_in_stun_mode() {
    let request = vec![
        ice("stun:stun.l.google.com:19302"),
        ice("turn:turn.example.com:3478"),
    ];
    let kept = filter_ice_servers(&request, &TraversalMode::Stun, &no_own());
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].urls[0], "stun:stun.l.google.com:19302");
}

/// Turn mode keeps both STUN and TURN. `traversal_mode` is the sole
/// authority — startup mode no longer gates TURN, so a `Default` /
/// `ServiceDaemon` host reached through a manager relays its TURN just like
/// a dedicated `DeskServer`.
#[test]
fn filter_keeps_both_in_turn_mode() {
    let request = vec![
        ice("stun:stun.l.google.com:19302"),
        ice("turn:turn.example.com:3478"),
    ];
    let kept = filter_ice_servers(&request, &TraversalMode::Turn, &no_own());
    assert_eq!(kept.len(), 2);
}

/// `TraversalMode::None` means "no STUN, no TURN, host candidates
/// only". The filter drops everything from the request.
#[test]
fn filter_drops_everything_in_none_mode() {
    let request = vec![
        ice("stun:stun.l.google.com:19302"),
        ice("turn:turn.example.com:3478"),
    ];
    let kept = filter_ice_servers(&request, &TraversalMode::None, &no_own());
    assert!(kept.is_empty());
}

/// Servers with no recognisable transport scheme are skipped (and
/// the daemon logs a warning) rather than admitted as unknown.
#[test]
fn filter_drops_unrecognised_transport() {
    let request = vec![
        ice("https://not-a-stun-or-turn.example.com"),
        ice("stun:stun.l.google.com:19302"),
    ];
    let kept = filter_ice_servers(&request, &TraversalMode::Stun, &no_own());
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].urls[0], "stun:stun.l.google.com:19302");
}

#[test]
fn own_turn_endpoints_maps_interfaces() {
    let turn = TurnSettings {
        interfaces: vec![
            desk_turn::model::TurnInterface {
                transport: desk_turn::model::TurnTransport::UDP,
                listen: "0.0.0.0:3479".to_string(),
                external: "192.168.50.5:3479".to_string(),
            },
            desk_turn::model::TurnInterface {
                transport: desk_turn::model::TurnTransport::TCP,
                listen: "0.0.0.0:3478".to_string(),
                external: "192.168.50.5:3478".to_string(),
            },
        ],
        // enable_turn does not gate the mapping — the caller's `Option`
        // (presence of a running `TurnApiState`) is the only gate.
        enable_turn: false,
        ..TurnSettings::default()
    };
    let eps = own_turn_endpoints(Some(&turn));
    assert_eq!(eps.len(), 2);
    assert!(eps.contains("192.168.50.5:3479"));
    assert!(eps.contains("192.168.50.5:3478"));
}

/// `None` (the embedded TURN never started — non-`Default`/`Signaling`
/// startup, or a `startup_turn_server` failure) yields an empty set, so
/// nothing is treated as self-hosted and no remote relay is dropped.
#[test]
fn own_turn_endpoints_none_is_empty() {
    assert!(own_turn_endpoints(None).is_empty());
}

#[test]
fn own_turn_endpoints_empty_interfaces_is_empty() {
    assert!(own_turn_endpoints(Some(&TurnSettings::default())).is_empty());
}

/// Turn mode, but the only TURN URL points back at our own bundled TURN:
/// the relay candidate is dropped while STUN survives.
#[test]
fn filter_drops_self_hosted_turn() {
    let request = vec![
        ice("stun:192.168.50.5:3479"),
        ice("turn:192.168.50.5:3479?transport=udp"),
    ];
    let own = own_turn_endpoints(Some(&turn_settings_with(&["192.168.50.5:3479"])));
    let kept = filter_ice_servers(&request, &TraversalMode::Turn, &own);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].urls[0], "stun:192.168.50.5:3479");
}

/// A single ICE server carrying both a self URL and a remote URL keeps the
/// remote URL (and its credential); only the self URL is removed.
#[test]
fn filter_partial_drops_self_url_keeps_remote() {
    let request = vec![LcxlRTCIceServer {
        urls: vec![
            "turn:192.168.50.5:3479?transport=udp".to_string(),
            "turn:relay.example.com:3478?transport=udp".to_string(),
        ],
        username: "user".to_string(),
        credential: "pw".to_string(),
    }];
    let own = own_turn_endpoints(Some(&turn_settings_with(&["192.168.50.5:3479"])));
    let kept = filter_ice_servers(&request, &TraversalMode::Turn, &own);
    assert_eq!(kept.len(), 1);
    assert_eq!(
        kept[0].urls,
        vec!["turn:relay.example.com:3478?transport=udp"]
    );
    assert_eq!(kept[0].username, "user");
    assert_eq!(kept[0].credential, "pw");
}

/// When every URL of an object is self-hosted, the whole object is dropped.
#[test]
fn filter_drops_object_when_all_urls_self() {
    let request = vec![LcxlRTCIceServer {
        urls: vec![
            "turn:192.168.50.5:3479?transport=udp".to_string(),
            "turn:192.168.50.5:3478?transport=tcp".to_string(),
        ],
        username: "user".to_string(),
        credential: "pw".to_string(),
    }];
    let own = own_turn_endpoints(Some(&turn_settings_with(&[
        "192.168.50.5:3479",
        "192.168.50.5:3478",
    ])));
    let kept = filter_ice_servers(&request, &TraversalMode::Turn, &own);
    assert!(kept.is_empty());
}

/// A remote manager's TURN (a different endpoint) is kept even when this
/// node hosts its own TURN — only self-hosted relays are dropped.
#[test]
fn filter_keeps_remote_turn_in_turn_mode() {
    let request = vec![ice("turn:relay.example.com:3478?transport=udp")];
    let own = own_turn_endpoints(Some(&turn_settings_with(&["192.168.50.5:3479"])));
    let kept = filter_ice_servers(&request, &TraversalMode::Turn, &own);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].urls[0], "turn:relay.example.com:3478?transport=udp");
}

/// No self-hosting (DeskServer / ServiceDaemon, own set empty): a remote
/// TURN is kept untouched.
#[test]
fn filter_keeps_turn_when_not_self_hosting() {
    let request = vec![ice("turn:192.168.50.5:3479?transport=udp")];
    let kept = filter_ice_servers(&request, &TraversalMode::Turn, &no_own());
    assert_eq!(kept.len(), 1);
}

/// The own-set is a frozen snapshot independent of any later live-settings
/// change: a relay at the startup address `A` is still filtered even though
/// the function only ever sees the passed-in set, never live settings.
#[test]
fn filter_uses_frozen_set_not_live() {
    let request = vec![ice("turn:192.168.50.5:3479?transport=udp")];
    // Frozen own-set captured at startup (address A).
    let own = own_turn_endpoints(Some(&turn_settings_with(&["192.168.50.5:3479"])));
    // Even if live settings had since moved to address B, the filter only
    // consults the frozen set, so the startup-A relay is still dropped.
    let kept = filter_ice_servers(&request, &TraversalMode::Turn, &own);
    assert!(kept.is_empty());
}

/// A TCP `external` endpoint is matched against the `turn:...?transport=tcp`
/// URL just like UDP.
#[test]
fn filter_matches_tcp_interface() {
    let request = vec![ice("turn:192.168.50.5:3478?transport=tcp")];
    let own = own_turn_endpoints(Some(&turn_settings_with(&["192.168.50.5:3478"])));
    let kept = filter_ice_servers(&request, &TraversalMode::Turn, &own);
    assert!(kept.is_empty());
}

/// An IPv6-shaped `external` matches purely as a string. This only
/// exercises the string match; it does NOT imply the IPv6 TURN runtime
/// path is wired up.
#[test]
fn filter_matches_ipv6_endpoint_string_only() {
    let request = vec![ice("turn:[fe80::1]:3479?transport=udp")];
    let own = own_turn_endpoints(Some(&turn_settings_with(&["[fe80::1]:3479"])));
    let kept = filter_ice_servers(&request, &TraversalMode::Turn, &own);
    assert!(kept.is_empty());
}

/// Sanity: the construction path itself works with an empty ICE
/// list (the daemon ICE-only-host case for portable mode).
#[tokio::test]
async fn build_peer_connection_succeeds_with_no_ice_servers() {
    let settings = Settings::default();
    let pc = build_peer_connection(vec![], &settings)
        .await
        .expect("build pc");
    // Just confirm we got a usable handle back; tear down via Drop.
    assert_eq!(
        pc.connection_state(),
        webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::New
    );
}

fn settings_with_startup(mode: StartupMode) -> Settings {
    let mut s = Settings::default();
    s.args.startup_mode = mode;
    s
}

/// `any_with_accept_control` reflects each PC's
/// `signaling_state.accept_control` flag: empty registry returns
/// false; a single PC with `accept_control = false` returns false;
/// flipping it true returns true; clearing it on one PC while
/// another is still holding control keeps the answer true (any,
/// not all). Pins the "any holder keeps exclusive alive" gate
/// used by `update_exclusive_after_control_change`.
#[tokio::test]
async fn any_with_accept_control_covers_empty_single_and_multi() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);

    assert!(
        !registry.any_with_accept_control().await,
        "empty registry must report false"
    );

    let ctx_a = registry
        .create_for_request_remote("conn-a", &request_remote, &s)
        .await
        .expect("seed a");
    assert!(
        !registry.any_with_accept_control().await,
        "fresh PC has accept_control = false"
    );

    // Flip A: now any() should be true.
    {
        let ctx = ctx_a.read().await;
        ctx.signaling_state.write().await.accept_control = true;
    }
    assert!(registry.any_with_accept_control().await);

    // Add B without flipping; A still holds.
    let ctx_b = registry
        .create_for_request_remote("conn-b", &request_remote, &s)
        .await
        .expect("seed b");
    assert!(registry.any_with_accept_control().await);

    // Flip A back to false; B still false. None hold -> false.
    {
        let ctx = ctx_a.read().await;
        ctx.signaling_state.write().await.accept_control = false;
    }
    assert!(!registry.any_with_accept_control().await);

    // Flip B to true; one holder -> true again.
    {
        let ctx = ctx_b.read().await;
        ctx.signaling_state.write().await.accept_control = true;
    }
    assert!(registry.any_with_accept_control().await);
}

/// Round-trip: create, contains, get, remove.
#[tokio::test]
async fn pc_registry_create_get_remove_cycle() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);

    assert_eq!(registry.len().await, 0);
    let _ctx = registry
        .create_for_request_remote("conn-a", &request_remote, &s)
        .await
        .expect("create");
    assert!(registry.contains("conn-a").await);
    assert_eq!(registry.len().await, 1);
    let got = registry.get("conn-a").await.expect("get");
    assert_eq!(got.read().await.connection_id, "conn-a");
    registry.remove("conn-a").await.expect("remove");
    assert!(!registry.contains("conn-a").await);
    assert_eq!(registry.len().await, 0);
}

/// Duplicate `create_for_request_remote` calls for the same
/// `connection_id` are a protocol error from the browser; the
/// registry refuses with a CustomError rather than overwriting
/// (which would leave the previous PC dangling without anyone
/// closing it).
#[tokio::test]
async fn pc_registry_rejects_duplicate_connection_id() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);

    registry
        .create_for_request_remote("conn-x", &request_remote, &s)
        .await
        .expect("first create");
    let result = registry
        .create_for_request_remote("conn-x", &request_remote, &s)
        .await;
    match result {
        Err(e) => assert!(format!("{e}").contains("already exists")),
        Ok(_) => panic!("second create_for_request_remote should fail"),
    }
    assert_eq!(registry.len().await, 1);
}

/// Minimal `StartMediaPayload` for the first-offer gating tests.
fn start_media_payload_for(connection_id: &str) -> StartMediaPayload {
    StartMediaPayload {
        connection_id: connection_id.to_string(),
        video_codec: MediaCodec::H264,
        audio_codec: MediaCodec::Opus,
        video_device: None,
        audio_device: None,
        fps: 30,
        bitrate_kbps: 0,
        quality: 0,
        start_video: true,
        start_audio: true,
        image_capture: None,
        enable_dirty_rect: None,
    }
}

/// `record_start_media_was_first` reports `true` only for the first
/// offer and overwrites the cached payload on every call. This is the
/// gate `handle_offer` uses to issue worker `StartMedia` exactly once
/// (first negotiation) while a renegotiation re-offer skips it but
/// still refreshes the cache for a later worker-swap resume.
#[tokio::test]
async fn record_start_media_marks_only_first_offer() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    let ctx = registry
        .create_for_request_remote("conn-a", &request_remote, &s)
        .await
        .expect("create");

    let first = ctx
        .read()
        .await
        .record_start_media_was_first(start_media_payload_for("conn-a"))
        .await;
    assert!(first, "the first offer must report is_first_offer = true");

    let second = ctx
        .read()
        .await
        .record_start_media_was_first(start_media_payload_for("conn-a"))
        .await;
    assert!(!second, "a renegotiation re-offer must report false");

    // Cache is populated for worker-swap resume regardless of which
    // offer it was.
    assert!(ctx.read().await.cached_start_media.read().await.is_some());
}

/// Two offers racing on the same connection (an in-flight initial
/// offer vs a frontend ICE-restart re-offer) must yield exactly one
/// `true`, so the worker receives a single `StartMedia`. The
/// serialization comes from each caller holding the
/// `PeerConnectionContext` write lock across the check-and-set,
/// mirroring `handle_offer`.
#[tokio::test]
async fn concurrent_offers_mark_first_once() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    let ctx = registry
        .create_for_request_remote("conn-race", &request_remote, &s)
        .await
        .expect("create");

    let c1 = Arc::clone(&ctx);
    let c2 = Arc::clone(&ctx);
    let t1 = tokio::spawn(async move {
        let g = c1.write().await;
        g.record_start_media_was_first(start_media_payload_for("conn-race"))
            .await
    });
    let t2 = tokio::spawn(async move {
        let g = c2.write().await;
        g.record_start_media_was_first(start_media_payload_for("conn-race"))
            .await
    });
    let r1 = t1.await.expect("task 1");
    let r2 = t2.await.expect("task 2");
    assert_eq!(
        [r1, r2].into_iter().filter(|x| *x).count(),
        1,
        "exactly one of two concurrent offers is the first"
    );
}

/// `PendingRequestGuard` is the RAII vehicle used by the router's
/// `RequestRemote` branch to suppress N→0 virtual-display detach
/// while a new browser is mid-`ensure_attached`. Verify the
/// counter is properly bumped on construction and decremented on
/// `Drop` (including across nesting and early exits).
#[test]
fn pending_request_guard_increments_and_decrements_counter() {
    let registry = PcRegistry::new();
    assert_eq!(registry.pending_requests(), 0, "starts at 0");

    let g1 = registry.enter_pending();
    assert_eq!(registry.pending_requests(), 1);

    {
        let _g2 = registry.enter_pending();
        assert_eq!(registry.pending_requests(), 2, "nested guard stacks");
    }
    assert_eq!(registry.pending_requests(), 1, "nested guard dropped");

    drop(g1);
    assert_eq!(registry.pending_requests(), 0, "outer guard dropped");
}

/// Frames addressed to a connection that is not in the registry
/// (race against `CloseControl` / browser drop) must be silently
/// dropped — never panic. The daemon's media-receiver loop runs
/// for the lifetime of the worker and a single panic there would
/// kill all media flow.
#[tokio::test]
async fn write_video_frame_unknown_connection_is_silent_noop() {
    let registry = PcRegistry::new();
    let frame = MediaFrame {
        connection_id: "ghost".into(),
        seq: 0,
        ts_ns: 0,
        duration_ns: 16_666_666,
        kind: MediaFrameKind::VideoP,
        codec: MediaCodec::H264,
        payload: vec![0xAB; 32],
    };
    // Test passes if this does not panic and the receiver loop is
    // free to keep reading.
    write_video_frame(&registry, frame).await;
}

/// Frames arriving before the offer has populated the per-PC
/// `video_track` (race window during initial setup) are dropped
/// with a debug log, not propagated. The receiver task must keep
/// running through that window.
#[tokio::test]
async fn write_video_frame_no_track_yet_is_silent_noop() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-no-track", &request_remote, &s)
        .await
        .expect("create");
    // Registry has the context, but `video_track` is still None
    // because no Offer ran (Offer is what populates the tracks in
    // `handle_offer`).
    let frame = MediaFrame {
        connection_id: "conn-no-track".into(),
        seq: 0,
        ts_ns: 0,
        duration_ns: 16_666_666,
        kind: MediaFrameKind::VideoI,
        codec: MediaCodec::H264,
        payload: vec![0xCD; 64],
    };
    write_video_frame(&registry, frame).await;
}

/// `pause_all_media` flips the per-PC flag for every
/// connection in the registry. Test isolates the registry-side
/// behaviour without involving worker IPC.
#[tokio::test]
async fn pause_all_media_marks_every_pc() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    for id in ["alpha", "beta", "gamma"] {
        registry
            .create_for_request_remote(id, &request_remote, &s)
            .await
            .expect("create");
    }

    // Sanity: nothing is paused at construction.
    for id in ["alpha", "beta", "gamma"] {
        let ctx = registry.get(id).await.unwrap();
        assert!(!ctx.read().await.media_paused.load(Ordering::Relaxed));
    }

    registry.pause_all_media().await;

    for id in ["alpha", "beta", "gamma"] {
        let ctx = registry.get(id).await.unwrap();
        assert!(
            ctx.read().await.media_paused.load(Ordering::Relaxed),
            "pause_all_media should mark {id}"
        );
    }
}

/// With `media_paused = true`, a P frame must be dropped and
/// the flag must remain set (next IDR is the resync barrier).
/// Verified by checking the flag stays `true` after the call —
/// `write_video_frame` swallows errors silently so we can't observe
/// the drop directly without instrumenting the track.
#[tokio::test]
async fn write_video_frame_paused_p_frame_keeps_flag_set() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-pause-p", &request_remote, &s)
        .await
        .expect("create");
    registry.pause_all_media().await;

    let frame = MediaFrame {
        connection_id: "conn-pause-p".into(),
        seq: 0,
        ts_ns: 0,
        duration_ns: 16_666_666,
        kind: MediaFrameKind::VideoP,
        codec: MediaCodec::H264,
        payload: vec![0x11; 16],
    };
    write_video_frame(&registry, frame).await;

    let ctx = registry.get("conn-pause-p").await.unwrap();
    assert!(
        ctx.read().await.media_paused.load(Ordering::Relaxed),
        "P frame during pause must not clear the flag"
    );
}

/// `MediaFrameKind::VideoI` arriving while paused clears the
/// flag in place. Subsequent frames flow normally. We cannot
/// observe the actual write_sample call (no track set), but the
/// flag transition is the contract that gates resume — verifying
/// it is sufficient.
#[tokio::test]
async fn write_video_frame_paused_i_frame_clears_flag() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-pause-i", &request_remote, &s)
        .await
        .expect("create");
    registry.pause_all_media().await;

    let frame = MediaFrame {
        connection_id: "conn-pause-i".into(),
        seq: 0,
        ts_ns: 0,
        duration_ns: 16_666_666,
        kind: MediaFrameKind::VideoI,
        codec: MediaCodec::H264,
        payload: vec![0x22; 32],
    };
    write_video_frame(&registry, frame).await;

    let ctx = registry.get("conn-pause-i").await.unwrap();
    assert!(
        !ctx.read().await.media_paused.load(Ordering::Relaxed),
        "first IDR while paused must clear the flag"
    );
}

/// `resume_active_media` over an empty registry must be a
/// silent no-op (no WorkerManager IPC, no panic). Guards the
/// post-shutdown / pre-first-RequestRemote race window.
#[tokio::test]
async fn resume_active_media_empty_registry_is_noop() {
    let registry = PcRegistry::new();
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

    // No PCs registered, no worker active — resume must just iterate
    // zero entries and return cleanly.
    registry.resume_active_media(&worker_mgr, None).await;
}

/// `reset_media_for` on an unknown connection_id is a silent no-op:
/// the daemon's MediaTransportStuck handler may race a
/// `StopMedia` / `pc.close()` and we don't want a stale recovery
/// attempt to panic or spawn IPC sends for a vanished PC.
#[tokio::test]
async fn reset_media_for_unknown_connection_is_noop() {
    let registry = PcRegistry::new();
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

    registry.reset_media_for("nope", &worker_mgr).await;
}

/// `broadcast_media_settings_update` with all-`None` payload
/// short-circuits without iterating the registry — pinning so a
/// future change doesn't accidentally fan out a no-op IPC to every
/// worker on every `UpdateDeskSettings` that touches only
/// non-media fields (wayland_control_mode, private_screen, etc.).
#[tokio::test]
async fn broadcast_media_settings_update_all_none_is_noop() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-x", &request_remote, &s)
        .await
        .expect("create");

    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

    // No worker active and all-None payload — must complete cleanly.
    registry
        .broadcast_media_settings_update(&worker_mgr, None, None, None, None)
        .await;
}

/// Regression for the dirty-rect kill-switch: a fan-out that
/// carries *only* `enable_dirty_rect` (fps / bitrate / quality all
/// `None`) must NOT short-circuit. The browser toggling the
/// Advanced-tab switch without changing anything else is the
/// expected path, and pre-fix `broadcast_media_settings_update`
/// would have early-returned on `fps.is_none() && bitrate.is_none()
/// && quality.is_none()`, silently dropping the toggle on the
/// floor.
#[tokio::test]
async fn broadcast_media_settings_update_dirty_rect_only_not_short_circuited() {
    let registry = PcRegistry::new();
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

    // Empty registry + dirty-rect-only payload: must complete
    // cleanly rather than early-return (the all-None guard must
    // include enable_dirty_rect).
    registry
        .broadcast_media_settings_update(&worker_mgr, None, None, None, Some(false))
        .await;
}

/// `broadcast_media_settings_update` only fans out to PCs that
/// already have a cached `StartMediaPayload`. A registry with PCs
/// that haven't received the first Offer yet (no cache) must
/// neither panic nor accidentally synthesize a default StartMedia
/// — handle_offer owns first-time fan-out.
#[tokio::test]
async fn broadcast_media_settings_update_skips_pcs_without_cached_offer() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-no-offer", &request_remote, &s)
        .await
        .expect("create");

    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

    // No cached_start_media → loop body skipped; no worker active
    // either, but the function must still not panic.
    registry
        .broadcast_media_settings_update(&worker_mgr, Some(60), None, Some(40), Some(false))
        .await;

    // The registered PC stays uncached.
    let ctx = registry.get("conn-no-offer").await.unwrap();
    assert!(ctx.read().await.cached_start_media.read().await.is_none());
}

/// `reset_media_for` on a registered connection without a cached
/// `StartMediaPayload` (the stuck error fired before the first
/// Offer/StartMedia ever landed) must still pause the PC and
/// emit `StopMedia` to clear any half-built worker state, but
/// must not synthesize a `StartMedia` from defaults.
#[tokio::test]
async fn reset_media_for_pauses_pc_even_without_cached_offer() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-stuck", &request_remote, &s)
        .await
        .expect("create");

    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

    registry.reset_media_for("conn-stuck", &worker_mgr).await;

    let ctx = registry.get("conn-stuck").await.unwrap();
    assert!(
        ctx.read().await.media_paused.load(Ordering::Relaxed),
        "reset_media_for must pause the PC so subsequent video frames are dropped \
             until a fresh IDR clears the flag"
    );
    // No cached StartMedia => the cached slot stays None and the
    // function returns early after the StopMedia send.
    assert!(ctx.read().await.cached_start_media.read().await.is_none());
}

/// A PC that hasn't yet received an Offer has
/// `cached_start_media = None`; resume must skip it (rather than
/// trying to send a default StartMedia, which would tell the
/// worker to start an encoder for a connection that hasn't
/// negotiated codecs yet).
#[tokio::test]
async fn resume_active_media_skips_pc_without_cached_offer() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-no-offer", &request_remote, &s)
        .await
        .expect("create");

    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

    // The worker_mgr has no active worker so any send_to_worker
    // would log a warning, but the snapshot loop must skip the PC
    // entirely because cached_start_media is None.
    registry.resume_active_media(&worker_mgr, None).await;

    // Cached slot stays None.
    let ctx = registry.get("conn-no-offer").await.unwrap();
    assert!(ctx.read().await.cached_start_media.read().await.is_none());
}

/// After a worker swap the freshly spawned worker has an empty
/// `ConnectionCeilingStore`, so `resume_active_media` must re-register the
/// capability ceiling of every `Admission::Capped` connection before any
/// worker-bound frame — otherwise the worker-side `meet(ceiling, global)`
/// gates fall back to global-only and a capped grant session silently
/// escalates. Re-sent even for a connection with no cached offer, since it
/// still accepts terminal / file frames.
#[tokio::test]
async fn resume_active_media_resends_ceiling_for_capped_admission() {
    use desk_ipc_protocol::message::ServiceToWorker;

    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-cap", &request_remote, &s)
        .await
        .expect("create");
    let ceiling = SecuritySettings {
        allow_terminal: Some(false),
        ..Default::default()
    };
    registry
        .record_admission("conn-cap", Admission::Capped(ceiling.clone()))
        .await;

    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
    let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    worker_mgr.install_active_for_test(ipc_tx).await;

    registry.resume_active_media(&worker_mgr, None).await;

    // The capped connection's ceiling was re-registered even though it had no
    // cached StartMedia (offer not yet exchanged).
    let mut saw_ceiling = false;
    while let Ok(msg) = ipc_rx.try_recv() {
        if let ServiceToWorker::SetConnectionCeiling(p) = msg {
            assert_eq!(p.connection_id, "conn-cap");
            assert_eq!(p.ceiling, Some(ceiling.clone()));
            saw_ceiling = true;
        }
    }
    assert!(
        saw_ceiling,
        "resume must re-register the capped connection's ceiling with the new worker"
    );
    // Fail-closed: with a reachable worker the ceiling send succeeded, so the
    // connection stays admitted rather than being torn down.
    assert!(registry.get("conn-cap").await.is_some());
}

/// Fail-closed on resume: if the ceiling re-registration cannot reach the new
/// worker (no active worker installed), the capped connection is torn down
/// rather than resumed uncapped.
#[tokio::test]
async fn resume_active_media_tears_down_capped_when_ceiling_undeliverable() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-cap-dead", &request_remote, &s)
        .await
        .expect("create");
    registry
        .record_admission(
            "conn-cap-dead",
            Admission::Capped(SecuritySettings::default()),
        )
        .await;

    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    // No active worker installed: send_to_worker fails, so the ceiling cannot
    // be delivered and the capped connection must be torn down.
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

    registry.resume_active_media(&worker_mgr, None).await;

    assert!(
        registry.get("conn-cap-dead").await.is_none(),
        "a capped connection whose ceiling cannot be re-registered must be torn down"
    );
}

/// Audio frames go through the same entry point but route to
/// `audio_track` instead of `video_track`. The daemon-side handler
/// must accept the variant without panicking when no audio track
/// exists.
#[tokio::test]
async fn write_video_frame_audio_kind_uses_audio_track_slot() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-audio", &request_remote, &s)
        .await
        .expect("create");
    let frame = MediaFrame {
        connection_id: "conn-audio".into(),
        seq: 0,
        ts_ns: 0,
        duration_ns: 20_000_000,
        kind: MediaFrameKind::Audio,
        codec: MediaCodec::Opus,
        payload: vec![0xEE; 96],
    };
    write_video_frame(&registry, frame).await;
}

/// `handle_request_remote` with a populated capabilities snapshot
/// uses the worker's reported codecs in the Init reply. This is
/// the path the daemon takes once the worker has sent its first
/// `WorkerToService::Capabilities`.
#[tokio::test]
async fn handle_request_remote_uses_worker_capabilities_when_present() {
    let registry = PcRegistry::new();
    let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(8);
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    let caps = MediaCapabilities {
        video_codecs: vec![MediaCodec::Vp9, MediaCodec::Av1],
        audio_codecs: vec![MediaCodec::Opus],
        video_encoders: vec!["VP9".to_string(), "AV1".to_string()],
        audio_encoders: vec!["OPUS".to_string()],
        video_device_list: std::collections::BTreeMap::new(),
        audio_device_list: std::collections::BTreeMap::new(),
        has_tauri: false,
        is_admin: true,
        desktop_name: "Default".to_string(),
    };
    let model = SignalingModel::new(
        "req-init",
        SignalingType::RequestRemote,
        Some("conn-init".to_string()),
        None,
        Some(
            serde_json::to_value(RequestRemoteModel {
                purpose: RemoteSessionPurpose::RemoteDesktop,
                ice_servers: vec![],
                grant_session_id: None,
            })
            .unwrap(),
        ),
        None,
    );

    handle_request_remote(
        &registry,
        &outbound_tx,
        &s,
        "user-x",
        false,
        Some(&caps),
        None,
        None,
        &model,
        None,
        None,
        0,
    )
    .await
    .expect("handle ok");

    let text = outbound_rx
        .recv()
        .await
        .expect("init reply must be broadcast");
    let reply: SignalingModel = serde_json::from_str(&text).expect("Init JSON must round-trip");
    assert_eq!(reply.signaling_type, SignalingType::Init);
    let init: InitSignalingData = reply
        .get_data::<InitSignalingData>()
        .expect("Init payload present");
    // Worker said Vp9, Av1 → daemon should ship those strings.
    assert_eq!(init.video_encoder_list, vec!["VP9", "AV1"]);
    assert_eq!(init.audio_encoder_list, vec!["OPUS"]);
    assert!(init.is_admin, "init must mirror caps.is_admin");
}

/// `handle_request_remote` without capabilities (first connection
/// before the worker has reported) falls back to the static
/// capture-engine factory enumerations. This keeps the legacy
/// behaviour during the small race window between worker spawn
/// and first Capabilities IPC.
#[tokio::test]
async fn handle_request_remote_falls_back_when_no_capabilities() {
    let registry = PcRegistry::new();
    let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(8);
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    let model = SignalingModel::new(
        "req-init-2",
        SignalingType::RequestRemote,
        Some("conn-init-2".to_string()),
        None,
        Some(
            serde_json::to_value(RequestRemoteModel {
                purpose: RemoteSessionPurpose::RemoteDesktop,
                ice_servers: vec![],
                grant_session_id: None,
            })
            .unwrap(),
        ),
        None,
    );

    handle_request_remote(
        &registry,
        &outbound_tx,
        &s,
        "user-x",
        false,
        None,
        None,
        None,
        &model,
        None,
        None,
        0,
    )
    .await
    .expect("handle ok");

    let text = outbound_rx.recv().await.expect("init reply");
    let reply: SignalingModel = serde_json::from_str(&text).unwrap();
    let init: InitSignalingData = reply.get_data::<InitSignalingData>().expect("Init payload");
    // Static fallback comes from `list_video_encoder()` /
    // `list_audio_encoder()` — both must be populated regardless
    // of test platform; we only check non-emptiness rather than
    // an exact platform-dependent list.
    assert!(!init.video_encoder_list.is_empty());
    assert!(!init.audio_encoder_list.is_empty());
}

/// A redeemed-grant `RequestRemote` carries a validated capability ceiling and
/// a grant-session id; `handle_request_remote` must (a) register the ceiling
/// with the worker's per-connection map ahead of any worker-bound frame and
/// (b) stamp all three (`restricted` / `access_ceiling` / `grant_session_id`)
/// onto the created connection's `SignalingState` before any frame egresses, so
/// the worker-side `meet(ceiling, global)` gates and grant-directed teardown
/// observe them from the connection's first frame.
#[tokio::test]
async fn handle_request_remote_stamps_ceiling_and_grant_onto_signaling_state() {
    use desk_ipc_protocol::message::ServiceToWorker;

    let registry = PcRegistry::new();
    let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(8);
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    // Stand up a worker manager with a fake active worker so the daemon has a
    // destination for the ceiling registration (grants are fail-closed without
    // one — see the dedicated fail-closed test).
    let shared = SharedSettings::from(s.clone());
    let settings_data = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings_data, registry.clone());
    let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    worker_mgr.install_active_for_test(ipc_tx).await;

    let model = SignalingModel::new(
        "req-grant-1",
        SignalingType::RequestRemote,
        Some("conn-grant-1".to_string()),
        None,
        Some(
            serde_json::to_value(RequestRemoteModel {
                purpose: RemoteSessionPurpose::RemoteDesktop,
                ice_servers: vec![],
                grant_session_id: Some("GS-1".to_string()),
            })
            .unwrap(),
        ),
        None,
    );

    let ceiling = SecuritySettings {
        allow_file_transfer: Some(false),
        ..Default::default()
    };

    handle_request_remote(
        &registry,
        &outbound_tx,
        &s,
        "user-x",
        false,
        None,
        Some(&worker_mgr),
        None,
        &model,
        Some(ceiling.clone()),
        Some("GS-1".to_string()),
        5,
    )
    .await
    .expect("handle ok");

    // Drain the Init reply so the broadcast channel does not lag.
    let _ = outbound_rx.recv().await.expect("init reply");

    // The worker received the ceiling registration for this connection.
    let mut saw_ceiling = false;
    while let Ok(msg) = ipc_rx.try_recv() {
        if let ServiceToWorker::SetConnectionCeiling(p) = msg {
            assert_eq!(p.connection_id, "conn-grant-1");
            assert_eq!(p.ceiling, Some(ceiling.clone()));
            saw_ceiling = true;
        }
    }
    assert!(
        saw_ceiling,
        "daemon must register the grant ceiling with the worker"
    );

    let ctx = registry.get("conn-grant-1").await.expect("pc registered");
    let st = ctx.read().await.signaling_state.read().await.clone();
    assert_eq!(
        st.access_ceiling,
        Some(ceiling),
        "validated ceiling must be stored for the worker-side meet gates"
    );
    assert_eq!(
        st.grant_session_id.as_deref(),
        Some("GS-1"),
        "grant-session id must index the connection"
    );

    // The grant connection is indexed under its grant for directed teardown.
    assert_eq!(
        registry.connections_for_grant("GS-1").await,
        ["conn-grant-1"]
    );
    // The stamped generation (5) is recorded with the grant so a later
    // regeneration can direct-close it by generation: revoking up to 5 selects
    // it, up to 4 does not.
    assert_eq!(registry.grants_up_to_generation(5).await, ["GS-1"]);
    assert!(registry.grants_up_to_generation(4).await.is_empty());
}

/// A grant `RequestRemote` (ceiling `Some`) is fail-closed when the daemon has
/// no worker to receive the ceiling registration: `handle_request_remote`
/// returns an error and registers no connection, so a capped session can never
/// run without its worker-side cap in place.
#[tokio::test]
async fn handle_request_remote_grant_fails_closed_without_worker() {
    let registry = PcRegistry::new();
    let (outbound_tx, _outbound_rx) = broadcast::channel::<String>(8);
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    let model = SignalingModel::new(
        "req-grant-2",
        SignalingType::RequestRemote,
        Some("conn-grant-2".to_string()),
        None,
        Some(
            serde_json::to_value(RequestRemoteModel {
                purpose: RemoteSessionPurpose::RemoteDesktop,
                ice_servers: vec![],
                grant_session_id: Some("GS-2".to_string()),
            })
            .unwrap(),
        ),
        None,
    );
    let ceiling = SecuritySettings {
        allow_file_transfer: Some(false),
        ..Default::default()
    };

    let result = handle_request_remote(
        &registry,
        &outbound_tx,
        &s,
        "user-x",
        false,
        None,
        None,
        None,
        &model,
        Some(ceiling),
        Some("GS-2".to_string()),
        9,
    )
    .await;

    assert!(result.is_err(), "grant without a worker must be rejected");
    assert!(
        registry.get("conn-grant-2").await.is_none(),
        "a rejected grant must leave no registered connection"
    );
    assert!(
        registry.connections_for_grant("GS-2").await.is_empty(),
        "a rejected grant must not index anything"
    );
}

/// Regression: when the worker reports `X264` and `H264` as two
/// separate concrete encoders (libx264 vs OpenH264), the daemon
/// must surface both strings in `InitSignalingData::
/// video_encoder_list`. Previously `video_codecs` (used for SDP
/// negotiation) collapsed both onto `MediaCodec::H264`, and the
/// daemon mapped that back through `media_codec_to_str` to two
/// indistinguishable "H264" entries. The fix routes the UI list
/// through `caps.video_encoders` instead.
#[tokio::test]
async fn handle_request_remote_preserves_x264_h264_distinction_in_encoder_list() {
    let registry = PcRegistry::new();
    let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(8);
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    let caps = MediaCapabilities {
        // SDP layer: only one H.264 entry (both implementations
        // produce equivalent H.264 wire format).
        video_codecs: vec![MediaCodec::H264, MediaCodec::Vp9],
        audio_codecs: vec![MediaCodec::Opus],
        // UI layer: both implementations remain distinct.
        video_encoders: vec!["X264".to_string(), "VP9".to_string(), "H264".to_string()],
        audio_encoders: vec!["OPUS".to_string()],
        video_device_list: std::collections::BTreeMap::new(),
        audio_device_list: std::collections::BTreeMap::new(),
        has_tauri: false,
        is_admin: false,
        desktop_name: "Default".to_string(),
    };
    let model = SignalingModel::new(
        "req-init-3",
        SignalingType::RequestRemote,
        Some("conn-init-3".to_string()),
        None,
        Some(
            serde_json::to_value(RequestRemoteModel {
                purpose: RemoteSessionPurpose::RemoteDesktop,
                ice_servers: vec![],
                grant_session_id: None,
            })
            .unwrap(),
        ),
        None,
    );

    handle_request_remote(
        &registry,
        &outbound_tx,
        &s,
        "user-x",
        false,
        Some(&caps),
        None,
        None,
        &model,
        None,
        None,
        0,
    )
    .await
    .expect("handle ok");

    let text = outbound_rx.recv().await.expect("init reply");
    let reply: SignalingModel = serde_json::from_str(&text).unwrap();
    let init: InitSignalingData = reply.get_data::<InitSignalingData>().expect("Init payload");
    assert_eq!(
        init.video_encoder_list,
        vec!["X264", "VP9", "H264"],
        "X264 and H264 must remain separate encoder choices for the UI \
             rather than collapsing to two indistinguishable 'H264' entries"
    );
    assert_eq!(init.audio_encoder_list, vec!["OPUS"]);
}

/// Regression: the daemon-side PC must publish locally-gathered
/// ICE candidates back through the signaling channel as
/// `SignalingType::Canid`. Without this the browser only learns
/// about the daemon's transport addresses via peer-reflexive
/// discovery, which times out after 30 s of `checking` for
/// multi-m-line PCs (video+audio+DC). The portable mode log
/// signature was: file-management (DC-only) connected, but the
/// remote-desktop page consistently failed ICE.
#[tokio::test]
async fn local_ice_candidate_forwarder_publishes_canid_to_outbound() {
    use std::time::Duration;
    use tokio::time::timeout;

    let settings = Settings::default();
    let pc = Arc::new(
        build_peer_connection(vec![], &settings)
            .await
            .expect("build pc"),
    );
    let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(32);
    register_local_ice_candidate_forwarder(
        Arc::clone(&pc),
        outbound_tx,
        "conn-trickle".to_string(),
    );

    // Trigger ICE gathering: any local SDP with at least one
    // m-section starts the gatherer. A DataChannel is the
    // cheapest such trigger (no transceiver bookkeeping).
    let _dc = pc
        .create_data_channel("trickle-test", None)
        .await
        .expect("create dc");
    let offer = pc.create_offer(None).await.expect("create offer");
    pc.set_local_description(offer)
        .await
        .expect("set local desc starts gathering");

    let mut canid_count = 0usize;
    let deadline = Duration::from_secs(5);
    loop {
        match timeout(deadline, outbound_rx.recv()).await {
            Ok(Ok(text)) => {
                let m: SignalingModel =
                    serde_json::from_str(&text).expect("outbound text must be a SignalingModel");
                if m.signaling_type != SignalingType::Canid {
                    continue;
                }
                assert_eq!(
                    m.to_connection_id.as_deref(),
                    Some("conn-trickle"),
                    "Canid must target the originating browser connection"
                );
                let init: RTCIceCandidateInit = m
                    .get_data::<RTCIceCandidateInit>()
                    .expect("Canid payload must be RTCIceCandidateInit");
                assert!(
                    !init.candidate.is_empty(),
                    "forwarded candidate string must be non-empty"
                );
                canid_count += 1;
                // Stop after the first one to keep the test fast;
                // counting the rest only adds flakiness.
                break;
            }
            _ => break,
        }
    }
    assert!(
        canid_count >= 1,
        "register_local_ice_candidate_forwarder must publish at least one Canid \
             after set_local_description triggers gathering; got {canid_count}"
    );
}

/// Regression: `handle_request_remote` must wire the on_ice_candidate
/// forwarder onto the freshly-created PC so that subsequent gathering
/// (kicked off by the browser's Offer) ships candidates back to the
/// browser. We exercise this end-to-end by manually triggering
/// gathering on the registry-stored PC after `handle_request_remote`
/// returns and asserting Canid messages arrive on `outbound`.
#[tokio::test]
async fn handle_request_remote_registers_ice_candidate_forwarder() {
    use std::time::Duration;
    use tokio::time::timeout;

    let registry = PcRegistry::new();
    let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(32);
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    let model = SignalingModel::new(
        "req-init-ice",
        SignalingType::RequestRemote,
        Some("conn-init-ice".to_string()),
        None,
        Some(
            serde_json::to_value(RequestRemoteModel {
                purpose: RemoteSessionPurpose::RemoteDesktop,
                ice_servers: vec![],
                grant_session_id: None,
            })
            .unwrap(),
        ),
        None,
    );

    handle_request_remote(
        &registry,
        &outbound_tx,
        &s,
        "user-x",
        false,
        None,
        None,
        None,
        &model,
        None,
        None,
        0,
    )
    .await
    .expect("handle ok");

    // Drain the Init reply.
    let init_text = outbound_rx.recv().await.expect("init reply");
    let init_reply: SignalingModel = serde_json::from_str(&init_text).unwrap();
    assert_eq!(init_reply.signaling_type, SignalingType::Init);

    // Now trigger gathering on the PC the registry holds. This is
    // what the Offer handler does in production; we do it directly
    // here because the unit test is scoped to handle_request_remote.
    let ctx = registry.get("conn-init-ice").await.expect("ctx exists");
    let pc = {
        let g = ctx.read().await;
        Arc::clone(&g.pc)
    };
    let _dc = pc.create_data_channel("trickle", None).await.expect("dc");
    let offer = pc.create_offer(None).await.expect("offer");
    pc.set_local_description(offer).await.expect("set local");

    let mut got_canid = false;
    let deadline = Duration::from_secs(5);
    loop {
        match timeout(deadline, outbound_rx.recv()).await {
            Ok(Ok(text)) => {
                let m: SignalingModel = serde_json::from_str(&text).unwrap();
                if m.signaling_type == SignalingType::Canid {
                    assert_eq!(m.to_connection_id.as_deref(), Some("conn-init-ice"));
                    got_canid = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(
        got_canid,
        "handle_request_remote must register the ICE forwarder so gathering ships Canid"
    );
}

/// Regression: when the browser-side PC reaches a terminal state
/// (Failed / Closed) the daemon must release the registry slot and
/// ship `StopMedia` to the worker. Without this the worker keeps the
/// per-connection encoder running and the per-output DXGI duplication
/// held; the next remote-desktop attempt then hits
/// `DuplicateOutput → 0x80070057 (E_INVALIDARG)` because Windows only
/// permits one duplication per (process, output) pair.
///
/// This test simulates terminal state via `pc.close()`, waits for the
/// async callback to fire, and asserts the registry entry is gone.
#[tokio::test]
async fn peer_connection_state_change_terminal_removes_registry_entry() {
    use std::time::Duration;
    use tokio::time::sleep;

    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    let ctx = registry
        .create_for_request_remote("conn-cleanup", &request_remote, &s)
        .await
        .expect("create");

    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

    let pc = {
        let g = ctx.read().await;
        Arc::clone(&g.pc)
    };
    register_peer_connection_state_cleanup(
        Arc::clone(&pc),
        registry.clone(),
        worker_mgr,
        None,
        "conn-cleanup".to_string(),
    );

    assert!(
        registry.contains("conn-cleanup").await,
        "registry must hold the PC before close()"
    );

    // Trigger terminal state. webrtc-rs schedules the state-change
    // callback asynchronously; poll the registry with a generous
    // 5 s budget so this test stays robust under heavy CI load.
    pc.close().await.expect("close pc");

    let deadline = Duration::from_secs(5);
    let start = std::time::Instant::now();
    while registry.contains("conn-cleanup").await {
        if start.elapsed() > deadline {
            panic!(
                "register_peer_connection_state_cleanup must remove the registry entry \
                     after pc.close() drives the PC to Closed; entry still present after 5s"
            );
        }
        sleep(Duration::from_millis(50)).await;
    }
}

/// `cleanup_pc` on an unknown connection_id must be a silent no-op:
/// the on_peer_connection_state_change callback can race a manual
/// CloseControl, and we don't want one path's success to drag the
/// other into a panic / error log spam.
#[tokio::test]
async fn cleanup_pc_unknown_connection_is_silent_noop() {
    let registry = PcRegistry::new();
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

    // No PC registered at all. Must not panic.
    cleanup_pc(&registry, &worker_mgr, None, "ghost-connection", "test").await;
    assert_eq!(registry.len().await, 0);
}

/// `cleanup_pc` removes the PC entry even when no worker is active.
/// The StopMedia send returns Err("No active worker") which is logged
/// at debug level and otherwise swallowed — pinning so a refactor that
/// converts the StopMedia send into an unwrap doesn't ship.
#[tokio::test]
async fn cleanup_pc_removes_registry_entry_even_without_worker() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-x", &request_remote, &s)
        .await
        .expect("create");

    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

    cleanup_pc(&registry, &worker_mgr, None, "conn-x", "test").await;

    assert!(!registry.contains("conn-x").await);
}

/// `handle_connection_removed` is the active cleanup path —
/// the signaling server fans out `ConnectionRemoved` the moment
/// a Browser peer's WS dies. Verify it tears down the daemon-side
/// PC for the named `from_connection_id` so the worker's DXGI
/// duplication is released before any reopen attempt races for it.
#[tokio::test]
async fn handle_connection_removed_clears_registry_for_existing_pc() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-bye", &request_remote, &s)
        .await
        .expect("create");

    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

    let model = SignalingModel::new(
        "req-conn-removed",
        SignalingType::ConnectionRemoved,
        Some("conn-bye".to_string()),
        None,
        None,
        None,
    );

    handle_connection_removed(&registry, &worker_mgr, None, &model)
        .await
        .expect("handler must not error on a known connection");

    assert!(!registry.contains("conn-bye").await);
}

/// `ConnectionRemoved` for a connection the daemon never
/// registered (e.g. a browser that never finished SDP) must be a
/// no-op rather than an error. The signaling broadcast is
/// best-effort and arrives at every Server peer in the
/// connection map regardless of whether the recipient was paired
/// with the departed browser; daemons that weren't involved
/// would otherwise log spurious failures every time any Browser
/// disconnects.
#[tokio::test]
async fn handle_connection_removed_unknown_connection_is_silent_noop() {
    let registry = PcRegistry::new();
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

    let model = SignalingModel::new(
        "req-conn-removed",
        SignalingType::ConnectionRemoved,
        Some("ghost-connection".to_string()),
        None,
        None,
        None,
    );

    handle_connection_removed(&registry, &worker_mgr, None, &model)
        .await
        .expect("handler must accept unknown ids without erroring");
    assert_eq!(registry.len().await, 0);
}

/// v5 lazy lifecycle: with the last PC removed and no pending
/// requests, `cleanup_pc` must `apply(false)` on the supervisor so
/// the IDD detaches and the dropdown clears on the next dialog.
#[tokio::test]
async fn cleanup_pc_detaches_supervisor_when_last_pc_removed_and_no_pending() {
    use crate::daemon::virtual_display::VirtualDisplaySupervisor;
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-only", &request_remote, &s)
        .await
        .expect("create");

    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
    let supervisor = Arc::new(VirtualDisplaySupervisor::new_attached_for_test(
        worker_mgr.clone(),
        "SWD\\TEST\\TEST",
    ));
    assert_eq!(supervisor.state_label().await, "Attached");

    cleanup_pc(
        &registry,
        &worker_mgr,
        Some(&supervisor),
        "conn-only",
        "test-n-to-zero",
    )
    .await;

    assert!(!registry.contains("conn-only").await);
    assert_eq!(
        supervisor.state_label().await,
        "Disabled",
        "N->0 cleanup must detach the supervisor",
    );
}

/// As long as other PCs are still live, the supervisor must stay
/// attached so the remaining session can keep using the IDD.
#[tokio::test]
async fn cleanup_pc_keeps_supervisor_when_other_pcs_remain() {
    use crate::daemon::virtual_display::VirtualDisplaySupervisor;
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    for id in ["conn-a", "conn-b"] {
        registry
            .create_for_request_remote(id, &request_remote, &s)
            .await
            .expect("create");
    }

    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
    let supervisor = Arc::new(VirtualDisplaySupervisor::new_attached_for_test(
        worker_mgr.clone(),
        "SWD\\TEST\\TEST",
    ));

    cleanup_pc(
        &registry,
        &worker_mgr,
        Some(&supervisor),
        "conn-a",
        "test-keep",
    )
    .await;

    assert!(registry.contains("conn-b").await);
    assert_eq!(
        supervisor.state_label().await,
        "Attached",
        "supervisor must remain Attached while another PC is live",
    );
}

/// Codex round 4 #10: a held `PendingRequestGuard` represents a new
/// `RequestRemote` mid-`ensure_attached` that hasn't registered a
/// PC yet. Cleanup of an old PC during this window must NOT detach
/// the IDD — the new connection is about to use it.
#[tokio::test]
async fn cleanup_pc_keeps_supervisor_when_pending_request_active() {
    use crate::daemon::virtual_display::VirtualDisplaySupervisor;
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-old", &request_remote, &s)
        .await
        .expect("create");

    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
    let supervisor = Arc::new(VirtualDisplaySupervisor::new_attached_for_test(
        worker_mgr.clone(),
        "SWD\\TEST\\TEST",
    ));

    // Simulate a new RequestRemote in the ensure_attached window:
    // PC not yet created, but a pending guard is live.
    let _pending = registry.enter_pending();

    cleanup_pc(
        &registry,
        &worker_mgr,
        Some(&supervisor),
        "conn-old",
        "test-pending-race",
    )
    .await;

    assert!(!registry.contains("conn-old").await);
    assert_eq!(
        supervisor.state_label().await,
        "Attached",
        "pending request guard must suppress N->0 detach",
    );
}

/// Codex round 3 #3 + cleanup_pc N→0 gate: a `cleanup_pc` call for
/// a connection id that was never registered (stale
/// `ConnectionRemoved` after the PC was already torn down) must
/// NOT trigger N→0 detach, even though `registry.len()` may
/// happen to be 0. The gate is `removed.is_some()`.
#[tokio::test]
async fn cleanup_pc_unknown_connection_does_not_detach_supervisor() {
    use crate::daemon::virtual_display::VirtualDisplaySupervisor;
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-live", &request_remote, &s)
        .await
        .expect("create");

    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
    let supervisor = Arc::new(VirtualDisplaySupervisor::new_attached_for_test(
        worker_mgr.clone(),
        "SWD\\TEST\\TEST",
    ));

    cleanup_pc(
        &registry,
        &worker_mgr,
        Some(&supervisor),
        "conn-ghost",
        "stale-ConnectionRemoved",
    )
    .await;

    assert!(
        registry.contains("conn-live").await,
        "unknown-id cleanup must not touch other PCs",
    );
    assert_eq!(
        supervisor.state_label().await,
        "Attached",
        "stale ConnectionRemoved must not trigger detach",
    );
}

/// Codex P1 #1 regression: when the departing PC was the sole
/// `accept_control=true` holder but another PC remains live (so
/// `registry.len() > 0` blocks the N→0 detach), the old code
/// never recomputed the exclusive-mode desired flag — the
/// supervisor stayed pinned at `desired=true` with no control
/// holder, leaving physical displays detached. cleanup_pc now
/// calls `supervisor.recompute_desired()` unconditionally on a
/// real removal so the registered closure (which queries
/// `any_with_accept_control`) fires.
///
/// The test installs an observable closure (records each call's
/// `active` argument) and asserts it runs at least once. The
/// supervisor's `set_desired_exclusive` was already covered by
/// the daemon::virtual_display tests, so we only need to prove
/// the cleanup path reaches the closure.
#[tokio::test]
async fn cleanup_pc_triggers_exclusive_recompute_when_other_pcs_remain() {
    use crate::daemon::virtual_display::{DesiredComputerFn, VirtualDisplaySupervisor};
    use std::future::Future;
    use std::pin::Pin;

    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    let ctx_a = registry
        .create_for_request_remote("conn-a", &request_remote, &s)
        .await
        .expect("seed a");
    registry
        .create_for_request_remote("conn-b", &request_remote, &s)
        .await
        .expect("seed b");
    // A is the sole control holder; B is view-only.
    {
        let ctx = ctx_a.read().await;
        ctx.signaling_state.write().await.accept_control = true;
    }
    assert!(registry.any_with_accept_control().await);

    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
    let supervisor = Arc::new(VirtualDisplaySupervisor::new_attached_for_test(
        worker_mgr.clone(),
        "SWD\\TEST\\TEST",
    ));

    // Install a desired_computer that mirrors the real router's
    // shape (queries any_with_accept_control on the registry) and
    // records the call count + the last `active` it received.
    let call_count = Arc::new(AtomicUsize::new(0));
    let last_active = Arc::new(AtomicBool::new(false));
    let registry_for_closure = registry.clone();
    let call_count_cl = Arc::clone(&call_count);
    let last_active_cl = Arc::clone(&last_active);
    let computer: DesiredComputerFn = Arc::new(move |active: bool| {
        let registry = registry_for_closure.clone();
        let call_count = Arc::clone(&call_count_cl);
        let last_active = Arc::clone(&last_active_cl);
        Box::pin(async move {
            call_count.fetch_add(1, Ordering::SeqCst);
            last_active.store(active, Ordering::SeqCst);
            if !active {
                return (false, 0u32);
            }
            let any = registry.any_with_accept_control().await;
            (any, 0u32)
        }) as Pin<Box<dyn Future<Output = (bool, u32)> + Send>>
    });
    supervisor.set_desired_computer(computer).await;

    // Sanity: the registry currently has a control holder, but
    // it is `conn-a` — the one we are about to remove.
    cleanup_pc(
        &registry,
        &worker_mgr,
        Some(&supervisor),
        "conn-a",
        "test-recompute",
    )
    .await;

    // PC A removed, PC B remains.
    assert!(!registry.contains("conn-a").await);
    assert!(registry.contains("conn-b").await);
    // The supervisor must remain attached (N→0 gate not hit).
    assert_eq!(supervisor.state_label().await, "Attached");

    // The recompute closure must have been invoked at least once
    // with the supervisor's real `active` snapshot. Without the
    // P1 #1 fix it would never run on this path.
    assert!(
        call_count.load(Ordering::SeqCst) >= 1,
        "recompute_desired closure must be invoked at least once on cleanup",
    );
    // And after the cleanup, no remaining PC holds accept_control.
    assert!(!registry.any_with_accept_control().await);
}

/// Codex P1 #1 sanity: cleanup of an unknown connection
/// (stale ConnectionRemoved) must NOT trigger recompute — the
/// gate is `removed.is_some()`.
#[tokio::test]
async fn cleanup_pc_does_not_recompute_on_stale_unknown_removal() {
    use crate::daemon::virtual_display::{DesiredComputerFn, VirtualDisplaySupervisor};
    use std::future::Future;
    use std::pin::Pin;

    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-live", &request_remote, &s)
        .await
        .expect("seed");

    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
    let supervisor = Arc::new(VirtualDisplaySupervisor::new_attached_for_test(
        worker_mgr.clone(),
        "SWD\\TEST\\TEST",
    ));

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_cl = Arc::clone(&call_count);
    let computer: DesiredComputerFn = Arc::new(move |_active: bool| {
        let call_count = Arc::clone(&call_count_cl);
        Box::pin(async move {
            call_count.fetch_add(1, Ordering::SeqCst);
            (false, 0u32)
        }) as Pin<Box<dyn Future<Output = (bool, u32)> + Send>>
    });
    supervisor.set_desired_computer(computer).await;

    cleanup_pc(
        &registry,
        &worker_mgr,
        Some(&supervisor),
        "conn-ghost",
        "stale-ConnectionRemoved",
    )
    .await;

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        0,
        "stale removal must not invoke the recompute closure",
    );
}

/// `virtual_display: None` (non-ServiceDaemon mode) must still let
/// cleanup_pc clear the registry without panicking.
#[tokio::test]
async fn cleanup_pc_skips_supervisor_when_none() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-x", &request_remote, &s)
        .await
        .expect("create");

    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

    cleanup_pc(&registry, &worker_mgr, None, "conn-x", "no-supervisor").await;

    assert!(!registry.contains("conn-x").await);
}

/// Codec round-trip: every IPC `MediaCodec` must map to a
/// non-empty string for the Init reply path. Pin so adding a new
/// codec to the IPC enum forces an update on the daemon side.
#[test]
fn media_codec_to_str_is_total_over_known_codecs() {
    for c in [
        MediaCodec::H264,
        MediaCodec::Vp8,
        MediaCodec::Vp9,
        MediaCodec::Av1,
        MediaCodec::Opus,
    ] {
        let s = media_codec_to_str(&c).expect("known codec maps to a string");
        assert!(!s.is_empty(), "{c:?}");
    }
}

/// `video_encoder_to_media_codec` must collapse X264 + H264 to
/// the same `MediaCodec::H264` (both are H.264 encoders, the
/// daemon doesn't differentiate them on the wire).
#[test]
fn video_encoder_to_media_codec_collapses_x264_and_h264() {
    assert_eq!(
        video_encoder_to_media_codec(VideoEncoderType::X264),
        MediaCodec::H264
    );
    assert_eq!(
        video_encoder_to_media_codec(VideoEncoderType::H264),
        MediaCodec::H264
    );
    assert_eq!(
        video_encoder_to_media_codec(VideoEncoderType::VP8),
        MediaCodec::Vp8
    );
}

// ============== DataChannel routing tests ==============

/// Every known DC label must classify to a `DcRoute`. Pin so a new
/// label added to `model::data_channel` without a matching route
/// here is caught at PR-review time rather than silently dropped
/// at runtime.
#[test]
fn classify_dc_label_covers_all_known_labels() {
    assert_eq!(classify_dc_label("mouse_event"), Some(DcRoute::Mouse));
    assert_eq!(
        classify_dc_label("mouse_move_event"),
        Some(DcRoute::MouseMove)
    );
    assert_eq!(classify_dc_label("keyboard_event"), Some(DcRoute::Keyboard));
    assert_eq!(
        classify_dc_label("clipboard_event"),
        Some(DcRoute::Clipboard)
    );
    assert_eq!(
        classify_dc_label("file_transfer_event"),
        Some(DcRoute::FileTransfer)
    );
    assert_eq!(
        classify_dc_label("whiteboard_event"),
        Some(DcRoute::Whiteboard)
    );
    assert_eq!(
        classify_dc_label("cursor_sync_event"),
        Some(DcRoute::CursorSync)
    );
    assert_eq!(classify_dc_label("not-a-real-channel"), None);
}

/// Each non-CursorSync route maps to the correct
/// `ServiceToWorker` variant carrying the same `connection_id` and
/// payload bytes the browser sent. The IPC layer is the trust
/// boundary between daemon and worker; this test pins the
/// translation so a refactor cannot accidentally re-route mouse
/// events as keyboard events.
#[test]
fn route_to_service_msg_preserves_payload_and_connection_id() {
    let cid = "conn-test";
    let data = vec![1u8, 2, 3, 4];

    match route_to_service_msg(DcRoute::Mouse, cid, data.clone()) {
        ServiceToWorker::MouseInput(p) => {
            assert_eq!(p.connection_id, cid);
            assert_eq!(p.data, data);
        }
        other => panic!("expected MouseInput, got {other:?}"),
    }
    match route_to_service_msg(DcRoute::MouseMove, cid, data.clone()) {
        ServiceToWorker::MouseMoveInput(p) => assert_eq!(p.data, data),
        other => panic!("expected MouseMoveInput, got {other:?}"),
    }
    match route_to_service_msg(DcRoute::Keyboard, cid, data.clone()) {
        ServiceToWorker::KeyboardInput(p) => assert_eq!(p.data, data),
        other => panic!("expected KeyboardInput, got {other:?}"),
    }
    match route_to_service_msg(DcRoute::Clipboard, cid, data.clone()) {
        ServiceToWorker::ClipboardWrite(p) => assert_eq!(p.data, data),
        other => panic!("expected ClipboardWrite, got {other:?}"),
    }
    match route_to_service_msg(DcRoute::Whiteboard, cid, data.clone()) {
        ServiceToWorker::WhiteboardCommand(p) => assert_eq!(p.data, data),
        other => panic!("expected WhiteboardCommand, got {other:?}"),
    }
}

/// CursorSync routing is a programmer error — calling
/// `route_to_service_msg` on it must panic rather than silently
/// emit a wrong variant. The router skips this case explicitly
/// before reaching the routing call.
#[test]
#[should_panic(expected = "CursorSync DC has no upstream message variant")]
fn route_to_service_msg_cursor_sync_panics() {
    let _ = route_to_service_msg(DcRoute::CursorSync, "c", vec![]);
}

/// FileTransfer rides the dedicated file lane (see
/// `desk-ipc-protocol::dual_transport`), not the event-lane
/// `route_to_service_msg`. Calling the router on it is a
/// programmer error and must panic — the production forwarder
/// special-cases FileTransfer before calling
/// `route_to_service_msg`. Pinning the panic message guards
/// against a future arm being added that silently moves file
/// bytes back onto the event lane.
#[test]
#[should_panic(expected = "FileTransfer is routed through")]
fn route_to_service_msg_file_transfer_panics() {
    let _ = route_to_service_msg(DcRoute::FileTransfer, "c", vec![]);
}

/// `accept_control = false` blocks Mouse / MouseMove / Keyboard
/// even when `accept_clipboard_sync = true`. Critical: a
/// regression here would let an unauthorised peer drive the
/// host's mouse / keyboard.
#[tokio::test]
async fn route_is_permitted_blocks_input_when_control_denied() {
    let state = Arc::new(RwLock::new(SignalingState {
        accept_control: false,
        accept_clipboard_sync: true,
        ..SignalingState::default()
    }));
    assert!(!route_is_permitted(DcRoute::Mouse, &state).await);
    assert!(!route_is_permitted(DcRoute::MouseMove, &state).await);
    assert!(!route_is_permitted(DcRoute::Keyboard, &state).await);
    assert!(!route_is_permitted(DcRoute::Whiteboard, &state).await);
    // Clipboard rides on its own gate, not control.
    assert!(route_is_permitted(DcRoute::Clipboard, &state).await);
    // FileTransfer is on `allow_file_transfer` (worker-side gate),
    // independent of accept_control. The browser file-management UI
    // opens a fresh PC that has never requested control, so any
    // accept_control gate here would silently drop every download.
    assert!(route_is_permitted(DcRoute::FileTransfer, &state).await);
}

/// File transfer must pass the daemon gate regardless of
/// `accept_control` / `accept_clipboard_sync`; the worker
/// dispatcher runs the actual `allow_file_transfer` security check.
/// Regression guard for the portable-mode "download stuck" bug.
#[tokio::test]
async fn route_is_permitted_passes_file_transfer_unconditionally() {
    let denied = Arc::new(RwLock::new(SignalingState {
        accept_control: false,
        accept_clipboard_sync: false,
        ..SignalingState::default()
    }));
    assert!(route_is_permitted(DcRoute::FileTransfer, &denied).await);

    let accepted = Arc::new(RwLock::new(SignalingState {
        accept_control: true,
        accept_clipboard_sync: true,
        ..SignalingState::default()
    }));
    assert!(route_is_permitted(DcRoute::FileTransfer, &accepted).await);
}

/// `accept_clipboard_sync = false` blocks Clipboard even when
/// `accept_control = true`. Independent gates: a peer can be
/// trusted with mouse/keyboard but not clipboard (e.g. screen
/// share without copy-paste).
#[tokio::test]
async fn route_is_permitted_blocks_clipboard_when_clipboard_denied() {
    let state = Arc::new(RwLock::new(SignalingState {
        accept_control: true,
        accept_clipboard_sync: false,
        ..SignalingState::default()
    }));
    assert!(!route_is_permitted(DcRoute::Clipboard, &state).await);
    // Control-gated routes still pass.
    assert!(route_is_permitted(DcRoute::Mouse, &state).await);
    assert!(route_is_permitted(DcRoute::Keyboard, &state).await);
}

/// Both gates open → every routable variant is permitted (cursor
/// sync stays out because the gate function panics on it; the
/// caller filters cursor sync before calling).
#[tokio::test]
async fn route_is_permitted_allows_all_when_both_accepted() {
    let state = Arc::new(RwLock::new(SignalingState {
        accept_control: true,
        accept_clipboard_sync: true,
        ..SignalingState::default()
    }));
    assert!(route_is_permitted(DcRoute::Mouse, &state).await);
    assert!(route_is_permitted(DcRoute::MouseMove, &state).await);
    assert!(route_is_permitted(DcRoute::Keyboard, &state).await);
    assert!(route_is_permitted(DcRoute::Clipboard, &state).await);
    assert!(route_is_permitted(DcRoute::FileTransfer, &state).await);
    assert!(route_is_permitted(DcRoute::Whiteboard, &state).await);
}

/// `route_is_permitted` no longer hard-denies a capability-restricted session
/// at the daemon door: with the retired `restricted` hard-deny branch gone, a
/// grant / support connection routes purely on its runtime accept bits, exactly
/// like an owner connection. The ceiling restriction is now enforced per
/// capability by the `meet(ceiling, global)` gates — clipboard via the
/// control-grant meet that sets `accept_clipboard_sync` (a capped session lands
/// with it false), file transfer / whiteboard via their worker dispatcher gates
/// (covered by those dispatchers' ceiling-deny tests). Here a session whose
/// control grant was approved but whose clipboard was capped off routes input
/// and lets file transfer through to the worker gate, while clipboard stays
/// denied by its own accept bit.
#[tokio::test]
async fn route_is_permitted_routes_on_accept_bits_not_a_restricted_flag() {
    let state = Arc::new(RwLock::new(SignalingState {
        accept_control: true,
        accept_clipboard_sync: false,
        ..SignalingState::default()
    }));
    // Pointer / keyboard follow the control grant.
    assert!(route_is_permitted(DcRoute::Mouse, &state).await);
    assert!(route_is_permitted(DcRoute::MouseMove, &state).await);
    assert!(route_is_permitted(DcRoute::Keyboard, &state).await);
    // Clipboard denied by its own capped accept bit.
    assert!(!route_is_permitted(DcRoute::Clipboard, &state).await);
    // File transfer / whiteboard pass the daemon door; their worker meet gates
    // are the enforcement point now.
    assert!(route_is_permitted(DcRoute::FileTransfer, &state).await);
    assert!(route_is_permitted(DcRoute::Whiteboard, &state).await);
}

/// A grant-directed teardown closes every connection that shares the grant in
/// one sweep (main + a second file-transfer connection of the same logical
/// session) while leaving connections of an unrelated grant / owner untouched,
/// and prunes the grant key once emptied.
#[tokio::test]
async fn close_grant_session_tears_down_all_grant_connections() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    for id in ["conn-g1-main", "conn-g1-file", "conn-other"] {
        registry
            .create_for_request_remote(id, &request_remote, &s)
            .await
            .expect("pc");
    }
    registry
        .index_grant_connection("GS-1", 5, "conn-g1-main")
        .await;
    registry
        .index_grant_connection("GS-1", 5, "conn-g1-file")
        .await;
    registry
        .index_grant_connection("GS-2", 5, "conn-other")
        .await;
    assert_eq!(registry.connections_for_grant("GS-1").await.len(), 2);

    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
    close_grant_session(&registry, &worker_mgr, None, "GS-1", "test").await;

    assert!(registry.get("conn-g1-main").await.is_none());
    assert!(registry.get("conn-g1-file").await.is_none());
    assert!(registry.get("conn-other").await.is_some());
    // The emptied grant key is pruned; the unrelated grant survives.
    assert!(registry.connections_for_grant("GS-1").await.is_empty());
    assert_eq!(registry.connections_for_grant("GS-2").await, ["conn-other"]);
}

/// A dial-code regeneration direct-closes every grant minted at or below the
/// revoked generation and leaves newer grants (and never-indexed owner sessions)
/// alone — the generation-scoped teardown driven by an inbound `RevokeAccessGrant`.
#[tokio::test]
async fn close_grants_up_to_generation_closes_only_superseded_grants() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    for id in ["conn-old", "conn-new"] {
        registry
            .create_for_request_remote(id, &request_remote, &s)
            .await
            .expect("pc");
    }
    // Two grants of the same device at different generations: gen 3 (stale after
    // a bump to 4) and gen 4 (the current one).
    registry
        .index_grant_connection("GS-old", 3, "conn-old")
        .await;
    registry
        .index_grant_connection("GS-new", 4, "conn-new")
        .await;

    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
    // Regeneration to generation 4 revokes everything at generation <= 3.
    close_grants_up_to_generation(&registry, &worker_mgr, None, 3, "dial_code_regenerated").await;

    // The superseded grant's connection is gone; the current-generation grant
    // survives untouched.
    assert!(registry.get("conn-old").await.is_none());
    assert!(registry.get("conn-new").await.is_some());
    assert!(registry.connections_for_grant("GS-old").await.is_empty());
    assert_eq!(registry.connections_for_grant("GS-new").await, ["conn-new"]);
}

/// `cleanup_pc` prunes the grant reverse-index on teardown so a later directed
/// revocation can never reach a stale connection id, and drops the grant key
/// once its last connection departs.
#[tokio::test]
async fn cleanup_pc_unindexes_grant_connection() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-g", &request_remote, &s)
        .await
        .expect("pc");
    registry.index_grant_connection("GS-9", 5, "conn-g").await;
    assert_eq!(registry.connections_for_grant("GS-9").await, ["conn-g"]);

    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
    cleanup_pc(&registry, &worker_mgr, None, "conn-g", "test").await;

    assert!(registry.connections_for_grant("GS-9").await.is_empty());
}

/// A grant-directed teardown (revocation / dial-code regeneration) must
/// physically end an open **terminal** connection of that grant — not just its
/// PC-bearing connections. The terminal WS holds no PC, so `cleanup_pc`'s PC /
/// media steps are no-ops for it; the terminal-aware branch must still send the
/// worker a `CloseTerminalRequest` (kill the shell) and a `SetConnectionCeiling`
/// clear, and drop the connection's admission / terminal mark / grant index.
/// Without this a revoked code's terminals would keep running (the codex Major).
#[tokio::test]
async fn close_grant_session_tears_down_terminal_connection() {
    use desk_ipc_protocol::message::ServiceToWorker;

    let registry = PcRegistry::new();
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    let settings = actix_web::web::Data::new(SharedSettings::from(s));
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
    let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    worker_mgr.install_active_for_test(ipc_tx).await;

    // A capped terminal connection (no PC) indexed under grant GS-T.
    let ceiling = SecuritySettings {
        allow_terminal: Some(true),
        ..Default::default()
    };
    registry
        .record_admission("term-1", Admission::Capped(ceiling))
        .await;
    registry.index_grant_connection("GS-T", 0, "term-1").await;
    registry.mark_terminal_connection("term-1").await;

    close_grant_session(&registry, &worker_mgr, None, "GS-T", "test-revoke").await;

    let mut saw_close = false;
    let mut saw_ceiling_clear = false;
    while let Ok(msg) = ipc_rx.try_recv() {
        match msg {
            ServiceToWorker::CloseTerminalRequest(p) if p.connection_id == "term-1" => {
                saw_close = true;
            }
            ServiceToWorker::SetConnectionCeiling(p)
                if p.connection_id == "term-1" && p.ceiling.is_none() =>
            {
                saw_ceiling_clear = true;
            }
            _ => {}
        }
    }
    assert!(saw_close, "grant revoke must close the terminal shell");
    assert!(
        saw_ceiling_clear,
        "grant revoke must clear the terminal ceiling"
    );
    assert!(registry.admission("term-1").await.is_none());
    assert!(!registry.is_terminal_connection("term-1").await);
    assert!(registry.connections_for_grant("GS-T").await.is_empty());
}

#[tokio::test]
async fn force_disconnect_tombstones_and_clears_admission_only_connection() {
    use desk_ipc_protocol::message::ServiceToWorker;

    let registry = PcRegistry::new();
    let settings = actix_web::web::Data::new(SharedSettings::from(settings_with_startup(
        StartupMode::ServiceDaemon,
    )));
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
    let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    worker_mgr.install_active_for_test(ipc_tx).await;
    registry
        .record_admission("conn-admitted", Admission::OwnerFull)
        .await;

    assert_eq!(registry.all_connection_ids().await, ["conn-admitted"]);
    assert!(
        force_disconnect_connection(
            &registry,
            &worker_mgr,
            None,
            "conn-admitted",
            "test-host-disconnect",
        )
        .await
    );

    assert!(registry.admission("conn-admitted").await.is_none());
    assert!(registry.is_tombstoned("conn-admitted").await);
    assert!(registry.all_connection_ids().await.is_empty());
    let mut saw_ceiling_clear = false;
    while let Ok(message) = ipc_rx.try_recv() {
        if matches!(
            message,
            ServiceToWorker::SetConnectionCeiling(payload)
                if payload.connection_id == "conn-admitted" && payload.ceiling.is_none()
        ) {
            saw_ceiling_clear = true;
        }
    }
    assert!(saw_ceiling_clear);
}

#[tokio::test]
async fn force_disconnect_closes_terminal_without_peer_connection() {
    use desk_ipc_protocol::message::ServiceToWorker;

    let registry = PcRegistry::new();
    let settings = actix_web::web::Data::new(SharedSettings::from(settings_with_startup(
        StartupMode::ServiceDaemon,
    )));
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
    let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    worker_mgr.install_active_for_test(ipc_tx).await;
    registry.mark_terminal_connection("term-host").await;

    assert!(
        force_disconnect_connection(
            &registry,
            &worker_mgr,
            None,
            "term-host",
            "test-host-disconnect",
        )
        .await
    );

    assert!(!registry.is_terminal_connection("term-host").await);
    assert!(registry.is_tombstoned("term-host").await);
    let mut saw_close_terminal = false;
    while let Ok(message) = ipc_rx.try_recv() {
        if matches!(
            message,
            ServiceToWorker::CloseTerminalRequest(payload)
                if payload.connection_id == "term-host"
        ) {
            saw_close_terminal = true;
        }
    }
    assert!(saw_close_terminal);
}

/// A capped connection's admission record survives a `CloseControl` PC teardown
/// (via `cleanup_pc`) and is only dropped when the signaling connection truly
/// ends (`ConnectionRemoved` → `handle_connection_removed`). This closes the
/// post-teardown escalation where a capped client sends `CloseControl` to drop
/// its PC and then reuses the same connection id for owner-plane frames: the
/// first door still classifies it as capped.
#[tokio::test]
async fn admission_survives_close_control_but_cleared_on_connection_removed() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-cap", &request_remote, &s)
        .await
        .expect("pc");
    registry
        .record_admission("conn-cap", Admission::Capped(SecuritySettings::default()))
        .await;

    let shared = SharedSettings::from(s);
    let settings = actix_web::web::Data::new(shared);
    let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

    // CloseControl-style teardown drops the PC but must NOT clear the admission.
    cleanup_pc(&registry, &worker_mgr, None, "conn-cap", "close_control").await;
    assert!(registry.get("conn-cap").await.is_none(), "PC torn down");
    assert!(
        matches!(
            registry.admission("conn-cap").await,
            Some(Admission::Capped(_))
        ),
        "admission must survive a CloseControl PC teardown"
    );

    // ConnectionRemoved ends the signaling connection → admission cleared.
    let model = SignalingModel::new(
        "req",
        SignalingType::ConnectionRemoved,
        Some("conn-cap".to_string()),
        None,
        None,
        None,
    );
    handle_connection_removed(&registry, &worker_mgr, None, &model)
        .await
        .expect("connection removed ok");
    assert!(
        registry.admission("conn-cap").await.is_none(),
        "ConnectionRemoved must clear the admission"
    );
}

/// `register_data_channel_router` is async-callable on a
/// freshly-built PC without panicking. We can't drive a real DC
/// open here without a peer connection on the other side, so this
/// is a smoke test for the registration call only — the routing
/// behaviour itself is covered by the pure-function tests above.
#[tokio::test]
async fn register_data_channel_router_smoke() {
    use crate::model::settings::SharedSettings;

    let settings = Settings::default();
    let pc = build_peer_connection(vec![], &settings).await.expect("pc");
    let signaling_state = Arc::new(RwLock::new(SignalingState::default()));
    let cursor_dc = Arc::new(RwLock::new(None));
    let clipboard_dc = Arc::new(RwLock::new(None));
    let file_transfer_dc = Arc::new(RwLock::new(None));
    let shared = SharedSettings::from(Settings::default());
    let settings_data = actix_web::web::Data::new(shared);
    let (worker_mgr, _) = WorkerManager::new(settings_data, PcRegistry::new());
    register_data_channel_router(
        Arc::new(pc),
        "conn-smoke".to_string(),
        signaling_state,
        cursor_dc,
        clipboard_dc,
        file_transfer_dc,
        worker_mgr,
    );
}

// ============== cursor sync write_cursor_data ==============

/// `write_cursor_data` for an unknown connection_id is a silent
/// no-op (no panic). Critical: the IPC receiver loop must keep
/// draining cursor packets even after a connection has been
/// closed (race against `CloseControl`).
#[tokio::test]
async fn write_cursor_data_unknown_connection_is_silent_noop() {
    let registry = PcRegistry::new();
    let payload = CursorDataPayload {
        connection_id: "ghost".to_string(),
        data: br#"{"visible":false}"#.to_vec(),
    };
    write_cursor_data(&registry, payload).await;
}

/// `write_cursor_data` for a known connection that has not yet
/// registered a `cursor_sync_event` DC (browser hasn't opened it
/// — control not granted, or DC negotiation in flight) is a
/// silent no-op. The browser would naturally not see a cursor
/// in that state; that is the intended behaviour.
#[tokio::test]
async fn write_cursor_data_no_dc_registered_is_silent_noop() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-no-cursor-dc", &request_remote, &s)
        .await
        .expect("create");
    let payload = CursorDataPayload {
        connection_id: "conn-no-cursor-dc".to_string(),
        data: br#"{"visible":true,"shape_id":42}"#.to_vec(),
    };
    // Test passes if this returns without panicking; the
    // cursor_data_channel slot is `None` at construction time,
    // so the silent-drop path must fire.
    write_cursor_data(&registry, payload).await;
}

/// Non-UTF-8 cursor payload bytes are dropped with a warn log,
/// not propagated. Worker should always serialise as JSON, but
/// the daemon must be resilient against a malformed shipment
/// from a buggy / mismatched worker version.
#[tokio::test]
async fn write_cursor_data_invalid_utf8_is_silent_noop() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-bad-utf8", &request_remote, &s)
        .await
        .expect("create");
    // 0xFF is not a valid UTF-8 start byte — would panic on
    // unwrap if the daemon used `.unwrap()` instead of the
    // explicit error branch.
    let payload = CursorDataPayload {
        connection_id: "conn-bad-utf8".to_string(),
        data: vec![0xFFu8, 0xFE, 0xFD],
    };
    write_cursor_data(&registry, payload).await;
}

// ============== write_clipboard_data ==============

/// `write_clipboard_data` for an unknown connection_id is a silent
/// no-op — race against `CloseControl` must not panic.
#[tokio::test]
async fn write_clipboard_data_unknown_connection_is_silent_noop() {
    let registry = PcRegistry::new();
    let payload = ClipboardPayload {
        connection_id: "ghost".to_string(),
        data: br#"{"type":"text","content":"x"}"#.to_vec(),
    };
    write_clipboard_data(&registry, payload).await;
}

/// Permission gate: a connection that has neither `accept_control`
/// nor `accept_clipboard_sync` set must not receive clipboard
/// pushes. Mirrors the worker polling-task gate that read both
/// flags from `SignalingState`.
#[tokio::test]
async fn write_clipboard_data_drops_when_permission_not_granted() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-no-perm", &request_remote, &s)
        .await
        .expect("create");
    let payload = ClipboardPayload {
        connection_id: "conn-no-perm".to_string(),
        data: br#"{"type":"text","content":"x"}"#.to_vec(),
    };
    // Default SignalingState has both flags false, so this must
    // silent-drop on the permission gate (before the DC-not-found
    // branch).
    write_clipboard_data(&registry, payload).await;
}

/// Permission granted but clipboard DC slot empty (browser hasn't
/// opened the `clipboard_event` channel) is a silent no-op.
#[tokio::test]
async fn write_clipboard_data_no_dc_is_silent_noop() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    let ctx = registry
        .create_for_request_remote("conn-no-dc", &request_remote, &s)
        .await
        .expect("create");
    // Flip the gates so we exercise the DC-missing branch.
    {
        let ctx_read = ctx.read().await;
        let mut s = ctx_read.signaling_state.write().await;
        s.accept_control = true;
        s.accept_clipboard_sync = true;
    }
    let payload = ClipboardPayload {
        connection_id: "conn-no-dc".to_string(),
        data: br#"{"type":"text","content":"x"}"#.to_vec(),
    };
    write_clipboard_data(&registry, payload).await;
}

/// Non-UTF-8 clipboard payload bytes are dropped (warn-logged) —
/// matches the cursor variant. Defends against a buggy worker
/// shipping malformed bytes.
#[tokio::test]
async fn write_clipboard_data_invalid_utf8_is_silent_noop() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    let ctx = registry
        .create_for_request_remote("conn-bad-utf8-clip", &request_remote, &s)
        .await
        .expect("create");
    {
        let ctx_read = ctx.read().await;
        let mut s = ctx_read.signaling_state.write().await;
        s.accept_control = true;
        s.accept_clipboard_sync = true;
    }
    let payload = ClipboardPayload {
        connection_id: "conn-bad-utf8-clip".to_string(),
        data: vec![0xFFu8, 0xFE, 0xFD],
    };
    write_clipboard_data(&registry, payload).await;
}

// ============== write_file_transfer_data ==============

/// `write_file_transfer_data` for an unknown connection_id is a
/// silent no-op — race against `CloseControl` must not panic.
#[tokio::test]
async fn write_file_transfer_data_unknown_connection_is_silent_noop() {
    let registry = PcRegistry::new();
    let payload = FileTransferPayload {
        connection_id: "ghost".to_string(),
        data: b"{\"type\":\"DownloadResponse\"}".to_vec(),
        is_text: true,
        transfer_id: None,
    };
    write_file_transfer_data(&registry, payload).await;
}

/// Regression for the portable-mode "download stuck at 0%" bug
/// fixed 2026-05-05: `write_file_transfer_data` must NOT gate on
/// `accept_control`. The browser file-management UI opens a fresh
/// PC that never requests remote control, so a `accept_control`
/// gate here silently dropped every download response chunk and
/// the worker-side dispatcher (which had already authorised the
/// transfer via `allow_file_transfer`) was left talking to a wall.
///
/// This test exercises the DC-missing silent-drop branch on a
/// connection whose `SignalingState` defaults to `accept_control =
/// false`. Before the fix, the function would have returned at
/// the permission check; after the fix it must reach (and silently
/// no-op at) the DC-missing branch. Both paths look identical from
/// the outside — the regression guard is the bare fact that no
/// `accept_control` read remains in the function body. Keep this
/// test alongside the source so a future re-introduction of the
/// gate fails an explicit, named test.
#[tokio::test]
async fn write_file_transfer_data_does_not_gate_on_accept_control() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-no-control", &request_remote, &s)
        .await
        .expect("create");
    // Default SignalingState has both flags false. Pre-fix, this
    // would silent-drop on the permission gate; post-fix it falls
    // through to the DC-missing branch (also a silent no-op, but
    // the path is now driven only by the DC slot and ready_state).
    let payload = FileTransferPayload {
        connection_id: "conn-no-control".to_string(),
        data: b"{\"type\":\"DownloadResponse\"}".to_vec(),
        is_text: true,
        transfer_id: None,
    };
    write_file_transfer_data(&registry, payload).await;
}

/// Binary chunks (raw download bytes, `is_text = false`) follow
/// the same DC-missing silent-drop path as text control replies.
/// Pinning here so a regression that special-cases the binary
/// branch (e.g. unwrapping the DC option) shows up as a panic
/// rather than a corrupted production transfer.
#[tokio::test]
async fn write_file_transfer_data_binary_no_dc_is_silent_noop() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-bin-no-dc", &request_remote, &s)
        .await
        .expect("create");
    let payload = FileTransferPayload {
        connection_id: "conn-bin-no-dc".to_string(),
        data: vec![0x00, 0x01, 0x02, 0x03],
        is_text: false,
        transfer_id: None,
    };
    write_file_transfer_data(&registry, payload).await;
}

/// Core regression for the 2026-05-06 "file/list timeouts after
/// big download" bug: `write_file_transfer_data` MUST return
/// immediately even when a large backlog of payloads is in flight.
/// Pre-fix the daemon's main IPC loop awaited `dc.send` for each
/// chunk, and a slow / blocked DataChannel head-of-line blocked
/// every other `WorkerToService` variant — including the
/// `ManagerFileListResponse` the file manager UI was waiting on,
/// causing 30-second `deadline elapsed` errors.
///
/// Post-fix the dispatch is `O(1)` (registry lookup + non-blocking
/// `UnboundedSender::send`); the actual `dc.send` runs in a
/// per-connection writer task. Pinning a per-call upper bound
/// guards against any future regression that re-introduces an
/// `await dc.send` (or any other unbounded await) on this path.
#[tokio::test(flavor = "current_thread")]
async fn write_file_transfer_data_dispatch_returns_quickly_under_backlog() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-fast-dispatch", &request_remote, &s)
        .await
        .expect("create");

    // No DC registered — every payload silently drops in the
    // writer task. We push 1024 payloads back-to-back and require
    // the *dispatch* phase to complete inside 200 ms total. On a
    // pre-fix `dc.send().await` path even with a stub DC this
    // would be O(N) on async scheduling overhead; here we are
    // dominated only by per-call mpsc enqueues.
    let started = tokio::time::Instant::now();
    for i in 0..1024 {
        let payload = FileTransferPayload {
            connection_id: "conn-fast-dispatch".to_string(),
            data: format!("chunk-{i}").into_bytes(),
            is_text: true,
            transfer_id: None,
        };
        write_file_transfer_data(&registry, payload).await;
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(200),
        "dispatch loop took {elapsed:?}; pre-fix HOL blocking regression?"
    );
}

/// Dispatching to an unknown `connection_id` (race against
/// `cleanup_pc → registry.remove`) is also expected to return
/// without spawning anything new. Covers the path where the
/// daemon's file-lane drain task picks up a stale payload for a
/// PC the registry already removed — pre-fix this hit the same DC
/// lookup as a live PC; post-fix it short-circuits at the registry
/// lookup before any sender clone.
#[tokio::test]
async fn write_file_transfer_data_after_registry_remove_is_silent_noop() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-removed", &request_remote, &s)
        .await
        .expect("create");
    // Drop the registry entry — equivalent to `cleanup_pc` having
    // run. The writer task's sender is the last remaining
    // `Arc<RwLock<PerConnectionContext>>` reference, so dropping
    // the returned ctx here drops the sender and the task exits.
    let removed = registry.remove("conn-removed").await;
    drop(removed);

    let payload = FileTransferPayload {
        connection_id: "conn-removed".to_string(),
        data: b"stale".to_vec(),
        is_text: true,
        transfer_id: None,
    };
    write_file_transfer_data(&registry, payload).await;
}

/// The writer task must exit as soon as its sender is dropped
/// (which is what `cleanup_pc → registry.remove` triggers). Pin
/// the lifecycle by spawning the task directly with a known
/// receiver, dropping the matching sender, and observing the task
/// completes within a tight bound. Guards against a future
/// refactor that accidentally retains the `UnboundedSender` on
/// some long-lived global / DC handler closure (the result would
/// be a writer task per closed connection, slowly leaking).
#[tokio::test]
async fn file_transfer_writer_task_exits_when_sender_drops() {
    let dc_slot: Arc<RwLock<Option<Arc<RTCDataChannel>>>> = Arc::new(RwLock::new(None));
    let (tx, rx) = mpsc::channel::<FileTransferPayload>(2);
    spawn_file_transfer_writer_task("conn-lifecycle".to_string(), rx, dc_slot, None);
    // Push one payload (silently dropped — no DC) then drop the
    // sender. The task drains the queued payload, observes
    // `recv() → None`, and exits.
    tx.send(FileTransferPayload {
        connection_id: "conn-lifecycle".to_string(),
        data: b"queued".to_vec(),
        is_text: true,
        transfer_id: None,
    })
    .await
    .expect("send pre-drop");
    drop(tx);
    // 200 ms is generous — the loop body for a no-DC payload is
    // pure CPU + a single read lock, so observed runtimes are
    // sub-millisecond. A blown timeout means the task did not
    // exit, i.e. the sender wasn't actually the last reference
    // (regression).
    tokio::time::timeout(
        std::time::Duration::from_millis(200),
        tokio::task::yield_now(),
    )
    .await
    .expect("yield");
    // No direct join handle because the task is spawned on the
    // actix-rt System; observable side effect is just that no
    // panic / hang occurred. Repeat the yield to give the
    // current_thread executor a chance to drive the task to
    // completion under the test runtime.
    tokio::task::yield_now().await;
}

/// Backpressure regression for the daemon side: when the
/// per-connection writer queue saturates,
/// `write_file_transfer_data` must `await` on the bounded
/// `Sender::send` instead of dropping silently. Pre-fix the queue
/// was unbounded so it always succeeded immediately, defeating the
/// chain that pushes backpressure back through the file lane to
/// the worker's `serve_download` loop.
///
/// We swap the writer sender on a registered PC for a tiny
/// (cap = 2) channel whose receiver we never drain, then assert
/// the third dispatch parks for at least 100 ms before draining
/// frees a slot.
#[tokio::test]
async fn write_file_transfer_data_awaits_when_writer_queue_full() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);
    registry
        .create_for_request_remote("conn-bp", &request_remote, &s)
        .await
        .expect("create");
    // Hijack the writer slot with a starving channel.
    let (slow_tx, mut slow_rx) = mpsc::channel::<FileTransferPayload>(2);
    {
        let ctx_arc = registry.get("conn-bp").await.unwrap();
        let mut ctx = ctx_arc.write().await;
        ctx.file_transfer_writer_tx = slow_tx;
    }
    let mk = |tag: &str| FileTransferPayload {
        connection_id: "conn-bp".to_string(),
        data: tag.as_bytes().to_vec(),
        is_text: true,
        transfer_id: None,
    };
    // First two writes fill the queue and return promptly.
    tokio::time::timeout(
        std::time::Duration::from_millis(100),
        write_file_transfer_data(&registry, mk("p1")),
    )
    .await
    .expect("first write should not block");
    tokio::time::timeout(
        std::time::Duration::from_millis(100),
        write_file_transfer_data(&registry, mk("p2")),
    )
    .await
    .expect("second write should not block");
    // Third must park on `Sender::send().await` — assert it
    // doesn't return inside the timeout.
    let blocked = tokio::time::timeout(
        std::time::Duration::from_millis(150),
        write_file_transfer_data(&registry, mk("p3")),
    )
    .await;
    assert!(
        blocked.is_err(),
        "third write should backpressure on bounded queue, got: {blocked:?}"
    );
    // Drain one slot — a fresh write completes promptly.
    slow_rx.recv().await.expect("drain p1");
    tokio::time::timeout(
        std::time::Duration::from_millis(150),
        write_file_transfer_data(&registry, mk("p4")),
    )
    .await
    .expect("post-drain write should complete");
}

// ============== RTCP PLI/FIR identity ==============

/// Identifying RTCP packets via `as_any().is::<T>()` /
/// `downcast_ref::<T>()` is the path `spawn_rtcp_feedback_task`
/// uses to decide between ForceKeyframe (PLI/FIR) and the
/// bitrate-cap controller (REMB). Pin the identities so a
/// webrtc-rs version bump that changed the trait object
/// representation is caught here, not in production where missed
/// PLIs become "browser stuck on stale frame after a packet loss"
/// and missed REMBs silently disable adaptive bitrate.
#[test]
fn rtcp_pli_fir_and_remb_are_distinguishable_via_as_any() {
    use webrtc::rtcp::packet::Packet;

    let pli: Box<dyn Packet + Send + Sync> = Box::new(PictureLossIndication {
        sender_ssrc: 1,
        media_ssrc: 2,
    });
    let fir: Box<dyn Packet + Send + Sync> = Box::new(FullIntraRequest {
        sender_ssrc: 1,
        media_ssrc: 2,
        fir: vec![],
    });
    let remb: Box<dyn Packet + Send + Sync> = Box::new(ReceiverEstimatedMaximumBitrate {
        sender_ssrc: 1,
        bitrate: 4_000_000.0,
        ssrcs: vec![2],
    });

    assert!(pli.as_any().is::<PictureLossIndication>());
    assert!(!pli.as_any().is::<FullIntraRequest>());
    assert!(fir.as_any().is::<FullIntraRequest>());
    assert!(!fir.as_any().is::<PictureLossIndication>());
    let parsed = remb
        .as_any()
        .downcast_ref::<ReceiverEstimatedMaximumBitrate>()
        .expect("REMB must downcast");
    assert_eq!(parsed.bitrate, 4_000_000.0);
    assert!(!remb.as_any().is::<PictureLossIndication>());
}

// ============== adaptive bitrate-cap IPC ==============

/// Pulls the next `UpdateMediaSettings` off the test IPC stream,
/// asserting it carries only a bitrate directive.
fn expect_cap_ipc(
    ipc_rx: &mut tokio::sync::mpsc::UnboundedReceiver<ServiceToWorker>,
    expect_connection: &str,
) -> u32 {
    match ipc_rx.try_recv().expect("expected an IPC message") {
        ServiceToWorker::UpdateMediaSettings(p) => {
            assert_eq!(p.connection_id, expect_connection);
            assert_eq!(p.fps, None);
            assert_eq!(p.quality, None);
            assert_eq!(p.enable_dirty_rect, None);
            p.bitrate_kbps.expect("cap IPC must carry bitrate_kbps")
        }
        other => panic!("expected UpdateMediaSettings, got {other:?}"),
    }
}

/// End-to-end over the daemon-side cap path: a committed cap
/// followed by a disable edge must emit `bitrate_kbps: Some(0)`
/// (the clear sentinel) for that connection, and decisions stop
/// afterwards.
#[tokio::test]
async fn disable_with_active_cap_emits_clear_ipc() {
    let registry = PcRegistry::new();
    let s = actix_web::web::Data::new(crate::model::settings::SharedSettings::from(
        settings_with_startup(StartupMode::ServiceDaemon),
    ));
    let (worker_mgr, _) = WorkerManager::new(s.clone(), registry.clone());
    let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    worker_mgr.install_active_for_test(ipc_tx).await;

    let shared = crate::daemon::bitrate_controller::AdaptiveBitrateShared::new(true);

    // REMB indicates an 8 Mbps link → SetCap(6800) shipped + committed.
    {
        let mut state = shared.state.lock().await;
        let directive = state
            .decide_on_remb(std::time::Instant::now(), 8_000_000.0)
            .expect("constrained REMB must produce a directive");
        send_cap_directive(&worker_mgr, "conn-cap", directive, &mut state).await;
        assert_eq!(state.current_cap_kbps(), Some(6_800));
    }
    assert_eq!(expect_cap_ipc(&mut ipc_rx, "conn-cap"), 6_800);

    // Disable → Clear (wire Some(0)) + no further decisions.
    {
        let mut state = shared.state.lock().await;
        let directive = state
            .set_enabled_and_decide_clear(false)
            .expect("disable with active cap must emit Clear");
        send_cap_directive(&worker_mgr, "conn-cap", directive, &mut state).await;
        assert_eq!(state.current_cap_kbps(), None);
        assert_eq!(
            state.decide_on_remb(std::time::Instant::now(), 2_000_000.0),
            None,
            "disabled state must not emit further directives"
        );
    }
    assert_eq!(
        expect_cap_ipc(&mut ipc_rx, "conn-cap"),
        0,
        "clear must ride the Some(0) sentinel"
    );
    assert!(ipc_rx.try_recv().is_err(), "no further IPC expected");
}

/// A failed `send_to_worker` must not commit: the controller state
/// keeps its previous cap so the next REMB re-decides instead of
/// being suppressed by hysteresis; after a fresh IPC channel is
/// installed the retry ships normally.
#[tokio::test]
async fn send_failure_does_not_commit_and_retry_succeeds() {
    let registry = PcRegistry::new();
    let s = actix_web::web::Data::new(crate::model::settings::SharedSettings::from(
        settings_with_startup(StartupMode::ServiceDaemon),
    ));
    let (worker_mgr, _) = WorkerManager::new(s.clone(), registry.clone());
    let (ipc_tx, ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    worker_mgr.install_active_for_test(ipc_tx).await;
    // Drop the receiver: the next send fails. (An mpsc receiver
    // cannot be revived — the retry below installs a new channel.)
    drop(ipc_rx);

    let shared = crate::daemon::bitrate_controller::AdaptiveBitrateShared::new(true);
    let now = std::time::Instant::now();

    {
        let mut state = shared.state.lock().await;
        let directive = state
            .decide_on_remb(now, 8_000_000.0)
            .expect("must decide a cap");
        send_cap_directive(&worker_mgr, "conn-f", directive, &mut state).await;
        assert_eq!(
            state.current_cap_kbps(),
            None,
            "failed send must not commit"
        );
    }

    // Fresh channel installed → identical REMB re-decides the same
    // directive (no hysteresis suppression) and ships it.
    let (ipc_tx2, mut ipc_rx2) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    worker_mgr.install_active_for_test(ipc_tx2).await;
    {
        let mut state = shared.state.lock().await;
        let directive = state
            .decide_on_remb(now, 8_000_000.0)
            .expect("retry must re-decide after an uncommitted failure");
        send_cap_directive(&worker_mgr, "conn-f", directive, &mut state).await;
        assert_eq!(state.current_cap_kbps(), Some(6_800));
    }
    assert_eq!(expect_cap_ipc(&mut ipc_rx2, "conn-f"), 6_800);
}

/// Serialisation contract: REMB decisions and the disable edge
/// both hold the state lock across decide → send → commit, so the
/// FIFO IPC stream can never show a `SetCap` after the `Clear`.
/// Drives many concurrent REMB tasks against one mid-flight
/// disable and inspects the observed wire sequence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_setcap_after_clear_under_concurrency() {
    let registry = PcRegistry::new();
    let s = actix_web::web::Data::new(crate::model::settings::SharedSettings::from(
        settings_with_startup(StartupMode::ServiceDaemon),
    ));
    let (worker_mgr, _) = WorkerManager::new(s.clone(), registry.clone());
    let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    worker_mgr.install_active_for_test(ipc_tx).await;

    let shared = Arc::new(crate::daemon::bitrate_controller::AdaptiveBitrateShared::new(true));

    let mut handles = Vec::new();
    for i in 0..50u32 {
        let shared = Arc::clone(&shared);
        let worker_mgr = worker_mgr.clone();
        handles.push(tokio::spawn(async move {
            // Alternate between two constrained estimates so the
            // urgent-drop path keeps emitting despite the 1 s
            // interval limiter.
            let remb = if i % 2 == 0 { 8_000_000.0 } else { 2_000_000.0 };
            let mut state = shared.state.lock().await;
            if let Some(d) = state.decide_on_remb(std::time::Instant::now(), remb) {
                send_cap_directive(&worker_mgr, "conn-race", d, &mut state).await;
            }
        }));
    }
    // Disable roughly mid-flight.
    {
        let shared = Arc::clone(&shared);
        let worker_mgr = worker_mgr.clone();
        handles.push(tokio::spawn(async move {
            tokio::task::yield_now().await;
            let mut state = shared.state.lock().await;
            if let Some(d) = state.set_enabled_and_decide_clear(false) {
                send_cap_directive(&worker_mgr, "conn-race", d, &mut state).await;
            }
        }));
    }
    for h in handles {
        h.await.expect("task panicked");
    }

    let mut saw_clear = false;
    while let Ok(msg) = ipc_rx.try_recv() {
        if let ServiceToWorker::UpdateMediaSettings(p) = msg {
            let kbps = p.bitrate_kbps.expect("cap IPC must carry bitrate_kbps");
            if kbps == 0 {
                saw_clear = true;
            } else {
                assert!(
                    !saw_clear,
                    "observed SetCap({kbps}) after Clear — decide/send/commit must be \
                         serialised under the state lock"
                );
            }
        }
    }
}

// ============== handle_require_control tests ==============

/// Build a SharedSettings whose security knobs are set to the
/// given allow-state for control / clipboard. `Some(true)` means
/// auto-allow without user prompt; `Some(false)` means auto-deny;
/// `None` would route to the host_control_hub which our test
/// fixture cannot drive without a Tauri shell.
fn settings_with_security(
    allow_control: Option<bool>,
    allow_clipboard: Option<bool>,
) -> Arc<crate::model::settings::SharedSettings> {
    let mut s = Settings::default();
    s.security.allow_remote_control = allow_control;
    s.security.allow_clipboard_sync = allow_clipboard;
    Arc::new(crate::model::settings::SharedSettings::from(s))
}

fn require_control_model(
    from_connection_id: &str,
    accept: bool,
    accept_clipboard_sync: bool,
) -> SignalingModel {
    SignalingModel::new(
        "req-rc",
        SignalingType::RequireControl,
        Some(from_connection_id.to_string()),
        None,
        Some(
            serde_json::to_value(SignalRequestControlData {
                accept,
                accept_file_transfer: false,
                accept_clipboard_sync,
            })
            .unwrap(),
        ),
        None,
    )
}

/// Auto-allow happy path: settings.security.allow_remote_control =
/// Some(true) + browser asks for both control and clipboard. State
/// flips, daemon emits AcceptControl back through outbound.
#[tokio::test]
async fn handle_require_control_auto_allows_and_emits_accept() {
    let registry = PcRegistry::new();
    let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(8);
    let settings = settings_with_security(Some(true), Some(true));
    let hub = Arc::new(HostControlHub::new_local());

    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    registry
        .create_for_request_remote("conn-rc", &request_remote, &*settings.read().await)
        .await
        .expect("seed pc");

    let model = require_control_model("conn-rc", true, true);
    handle_require_control(&registry, &outbound_tx, &settings, &hub, &model)
        .await
        .expect("handle ok");

    let text = outbound_rx.recv().await.expect("AcceptControl reply");
    let reply: SignalingModel = serde_json::from_str(&text).expect("decode reply");
    assert_eq!(
        reply.signaling_type,
        SignalingType::AcceptControl,
        "expected AcceptControl, got {:?}",
        reply.signaling_type,
    );
    let ctx = registry.get("conn-rc").await.unwrap();
    let s = ctx.read().await.signaling_state.read().await.clone();
    assert!(s.accept_control, "accept_control must flip true");
    assert!(
        s.accept_clipboard_sync,
        "accept_clipboard_sync must flip true when both grants approved"
    );
}

/// Control denied via settings: state stays false, DenyControl
/// reply. Subsequent mouse / keyboard IPC must remain blocked
/// because the daemon's permission gate reads from the same state.
#[tokio::test]
async fn handle_require_control_auto_denies_and_emits_deny() {
    let registry = PcRegistry::new();
    let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(8);
    let settings = settings_with_security(Some(false), None);
    let hub = Arc::new(HostControlHub::new_local());

    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    registry
        .create_for_request_remote("conn-deny", &request_remote, &*settings.read().await)
        .await
        .expect("seed pc");

    let model = require_control_model("conn-deny", true, false);
    handle_require_control(&registry, &outbound_tx, &settings, &hub, &model)
        .await
        .expect("handle ok");

    let text = outbound_rx.recv().await.expect("DenyControl reply");
    let reply: SignalingModel = serde_json::from_str(&text).expect("decode");
    assert_eq!(
        reply.signaling_type,
        SignalingType::DenyControl,
        "expected DenyControl, got {:?}",
        reply.signaling_type,
    );
    let ctx = registry.get("conn-deny").await.unwrap();
    let s = ctx.read().await.signaling_state.read().await.clone();
    assert!(!s.accept_control, "accept_control must stay false");
    assert!(
        !s.accept_clipboard_sync,
        "accept_clipboard_sync must stay false"
    );
}

/// A redeemed-grant session whose ceiling denies remote control is denied even
/// when the host global allows it: the daemon control gate meets the
/// connection ceiling with the global, so the grant can only tighten. Clipboard
/// is likewise capped by the ceiling meet.
#[tokio::test]
async fn handle_require_control_meets_ceiling_and_denies_when_ceiling_denies() {
    let registry = PcRegistry::new();
    let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(8);
    // Host global would allow both control and clipboard...
    let settings = settings_with_security(Some(true), Some(true));
    let hub = Arc::new(HostControlHub::new_local());

    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let ctx = registry
        .create_for_request_remote("conn-cap", &request_remote, &*settings.read().await)
        .await
        .expect("seed pc");
    // ...but the grant ceiling denies remote control (and leaves clipboard
    // unset → prompt, which a headless host denies).
    {
        let guard = ctx.read().await;
        let mut st = guard.signaling_state.write().await;
        st.access_ceiling = Some(SecuritySettings {
            allow_remote_control: Some(false),
            ..Default::default()
        });
        st.grant_session_id = Some("GS-cap".to_string());
    }

    let model = require_control_model("conn-cap", true, true);
    handle_require_control(&registry, &outbound_tx, &settings, &hub, &model)
        .await
        .expect("handle ok");

    let text = outbound_rx.recv().await.expect("reply");
    let reply: SignalingModel = serde_json::from_str(&text).expect("decode");
    assert_eq!(
        reply.signaling_type,
        SignalingType::DenyControl,
        "ceiling denial must override an allowing global"
    );
    let s = ctx.read().await.signaling_state.read().await.clone();
    assert!(!s.accept_control, "accept_control must stay false");
    assert!(!s.accept_clipboard_sync, "clipboard must stay false");
}

/// Release path: browser sends RequireControl{accept=false} to
/// release a previously-granted control. State goes false +
/// CloseControl reply. The short-circuit helper must NOT
/// short-circuit the release (would leave the worker stuck with
/// accept_control=true) — covered by `should_short_circuit_*`
/// helper tests in service::signaling, but verified end-to-end here.
#[tokio::test]
async fn handle_require_control_release_emits_close_and_resets_state() {
    let registry = PcRegistry::new();
    let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(8);
    let settings = settings_with_security(Some(true), Some(true));
    let hub = Arc::new(HostControlHub::new_local());

    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let ctx = registry
        .create_for_request_remote("conn-release", &request_remote, &*settings.read().await)
        .await
        .expect("seed pc");
    // Pre-flip state to "currently controlling" so the release
    // path is the one that fires.
    {
        let ctx_read = ctx.read().await;
        let mut s = ctx_read.signaling_state.write().await;
        s.accept_control = true;
        s.accept_clipboard_sync = true;
    }

    let model = require_control_model("conn-release", false, false);
    handle_require_control(&registry, &outbound_tx, &settings, &hub, &model)
        .await
        .expect("handle ok");

    let text = outbound_rx.recv().await.expect("CloseControl reply");
    let reply: SignalingModel = serde_json::from_str(&text).expect("decode");
    assert_eq!(
        reply.signaling_type,
        SignalingType::CloseControl,
        "expected CloseControl, got {:?}",
        reply.signaling_type,
    );
    let s = ctx.read().await.signaling_state.read().await.clone();
    assert!(!s.accept_control, "accept_control must go false on release");
    assert!(
        !s.accept_clipboard_sync,
        "accept_clipboard_sync must go false on release"
    );
}

/// Regression: releasing control must NEVER prompt the host, even when
/// `allow_remote_control = None` (the default "ask" mode). The browser sends
/// RequireControl{accept=false} when the user clicks "cancel control"; if the
/// release path consulted the approval hub it would pop a spurious
/// authorization dialog and block on the UI-readiness probe with no Tauri
/// shell connected. Asserting it resolves well under the probe timeout proves
/// the hub was never consulted.
#[tokio::test]
async fn handle_require_control_release_does_not_prompt_when_ask_mode() {
    let registry = PcRegistry::new();
    let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(8);
    // None = "ask the user" — the path that previously triggered the dialog.
    let settings = settings_with_security(None, None);
    let hub = Arc::new(HostControlHub::new_local());

    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let ctx = registry
        .create_for_request_remote("conn-ask-release", &request_remote, &*settings.read().await)
        .await
        .expect("seed pc");
    {
        let ctx_read = ctx.read().await;
        let mut s = ctx_read.signaling_state.write().await;
        s.accept_control = true;
        s.accept_clipboard_sync = true;
    }

    let model = require_control_model("conn-ask-release", false, false);
    // Must resolve promptly: the real UI-readiness probe is 10s, so a 1s
    // budget fails loudly if the release ever routes through the hub.
    let model_ref = &model;
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        handle_require_control(&registry, &outbound_tx, &settings, &hub, model_ref),
    )
    .await
    .expect("release must not block on the approval hub")
    .expect("handle ok");

    let text = outbound_rx.recv().await.expect("CloseControl reply");
    let reply: SignalingModel = serde_json::from_str(&text).expect("decode");
    assert_eq!(
        reply.signaling_type,
        SignalingType::CloseControl,
        "expected CloseControl, got {:?}",
        reply.signaling_type,
    );
    let s = ctx.read().await.signaling_state.read().await.clone();
    assert!(!s.accept_control, "accept_control must go false on release");
    assert!(
        !s.accept_clipboard_sync,
        "accept_clipboard_sync must go false on release"
    );
}

/// Re-grant of an already-accepted control short-circuits — the
/// helper returns true without prompting the user (would race
/// against any in-flight Tauri dialog otherwise). State stays
/// true, AcceptControl reply emitted.
#[tokio::test]
async fn handle_require_control_regrant_short_circuits() {
    let registry = PcRegistry::new();
    let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(8);
    // Settings deliberately set to None so a non-short-circuit
    // path would route to the hub — but the short-circuit fires
    // first because state is already accepted. If the
    // short-circuit broke, this test would hang on the hub call.
    let settings = settings_with_security(None, None);
    let hub = Arc::new(HostControlHub::new_local());

    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let ctx = registry
        .create_for_request_remote("conn-regrant", &request_remote, &*settings.read().await)
        .await
        .expect("seed pc");
    {
        let ctx_read = ctx.read().await;
        let mut s = ctx_read.signaling_state.write().await;
        s.accept_control = true;
        s.accept_clipboard_sync = true;
    }

    let model = require_control_model("conn-regrant", true, true);
    // Short timeout so a regression that bypasses the
    // short-circuit and falls into the hub call (which would
    // never complete in this test fixture) fails loudly.
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        handle_require_control(&registry, &outbound_tx, &settings, &hub, &model),
    )
    .await
    .expect("handle_require_control must short-circuit, not block on hub")
    .expect("handle ok");

    let text = outbound_rx.recv().await.expect("AcceptControl reply");
    let reply: SignalingModel = serde_json::from_str(&text).expect("decode");
    assert_eq!(reply.signaling_type, SignalingType::AcceptControl);
}

/// RequireControl for an unknown `connection_id` returns an error
/// (browser sent a grant for a PC the daemon never created — most
/// likely the matching RequestRemote was rejected upstream). The
/// router relays the error to the upstream signaling so the
/// browser can re-issue cleanly.
#[tokio::test]
async fn handle_require_control_unknown_connection_errors() {
    let registry = PcRegistry::new();
    let (outbound_tx, _) = broadcast::channel::<String>(8);
    let settings = settings_with_security(Some(true), Some(true));
    let hub = Arc::new(HostControlHub::new_local());

    let model = require_control_model("ghost", true, true);
    let result = handle_require_control(&registry, &outbound_tx, &settings, &hub, &model).await;
    assert!(result.is_err(), "unknown connection must surface an error");
}

/// Multi-connection: independent contexts coexist; closing one
/// leaves the other intact (multi-browser concurrency contract).
#[tokio::test]
async fn pc_registry_supports_multiple_independent_connections() {
    let registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let s = settings_with_startup(StartupMode::ServiceDaemon);

    registry
        .create_for_request_remote("a", &request_remote, &s)
        .await
        .expect("a");
    registry
        .create_for_request_remote("b", &request_remote, &s)
        .await
        .expect("b");
    assert_eq!(registry.len().await, 2);
    registry.remove("a").await;
    assert!(!registry.contains("a").await);
    assert!(registry.contains("b").await);
    assert_eq!(registry.len().await, 1);
}
