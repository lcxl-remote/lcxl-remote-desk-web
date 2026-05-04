# Arch IV — DeskServer mode migration + service/signaling Arch III cleanup

## Background

After PR 7 sign-off the Arch IV cutover was structurally complete on
`feat/arch-iv-daemon-webrtc` *except* for two items the PR 7 archive
explicitly deferred (see
`agent_works/web/2026-05-04_arch-iv-pr7-cleanup.md`, "Notes for the next
session"):

1. `StartupMode::DeskServer` still ran the legacy `start_desk_session`
   path with the WebRTC PeerConnection owned by `DeskSession` itself
   (Arch III).
2. The Arch III PeerConnection / SDP / ICE / capture code in
   `service/signaling.rs` was kept around because (1) still depended on
   it.

Both items are now done. Browser-facing topology in DeskServer mode
becomes identical to portable Default and ServiceDaemon: the daemon
owns the PC, the worker (in-process for both Default and DeskServer)
runs capture + encode + inject.

## Step A1 — DeskServer routed through in-process daemon

`web/server/src/lib.rs` now folds `StartupMode::DeskServer` into the
same `match` arm as `StartupMode::Default`, both spawning
`daemon::start_inprocess_daemon`. The in-process daemon's signaling
proxy already gates its `local_handle` on `startup_mode`
(`StartupMode::Default | StartupMode::ServiceDaemon`), so DeskServer
naturally runs only the **remote signaling + remote manager** WS
clients — matching the headless desk-node role.

Files touched:

- `web/server/src/lib.rs`
  - Drop `use service::signaling::start_desk_session;`.
  - Collapse `Default` + `DeskServer` arms into one calling
    `daemon::start_inprocess_daemon`.
- `web/server/src/daemon/mod.rs`
  - Generalize `start_inprocess_daemon` doc to cover both
    Default (portable) and DeskServer (headless), pointing at
    `signaling_proxy`'s mode-aware local-WS gating.

## Step A2 — Delete Arch III remnants in `service/signaling.rs`

Now that no startup mode imports the legacy WS-driven
`DeskSession::init_ptc_peer_connection` / `start_webrtc` /
`capture_screen_task` / `capture_audio_task` / `handle_offer` paths,
they all become dead code. `service/signaling.rs` shrinks from
**2221 → 475 lines** by deleting:

- `start_desk_session` + `maintain_signaling_connection`
- `handle_incoming_ws_message` + `handle_outgoing_channel_message`
- `PeerConnection` struct (`rtc_peer_connection`,
  `capture_screen_thread`, `capture_audio_thread`, `signaling_state`,
  `cursor_data_channel`)
- `DeskSession` fields `rtc_peer_connection_map` +
  `update_setting_sender`
- `DeskSession::{init_ptc_peer_connection, start_webrtc, handle_offer,
  capture_screen_task, capture_audio_task, get_rtc_peer_connection,
  binary, ping, handle_request_control}`
- `handle_message` arms for `RequestRemote` / `Offer` / `Answer` /
  `Canid` / `RequireControl` / `CloseControl`
- `handle_update_desk_settings` (its only effect was poking the
  capture loop, which no longer lives here)
- `ConnectionStateChangeResult` + `handle_connection_state_change`
- `DeskSessionMessage::WebRTCDropped` variant
- `CAPTURE_SCREEN_HISTOGRAM` + `WEBRTC_WRITE_SAMPLE_HISTOGRAM`
- All the `webrtc::*` / `desk_capture_engine::*` /
  `desk_signal_facade::model::desk_settings` /
  `RemoteDeskTypeEnum` / `awc::*` / `rustls::*` / etc imports that
  the deletions stranded.

Kept (still used by daemon code or worker session):

- `LocalNodeTokenValidator`
- `DeskSessionMessage::{Text, Binary, Ping, Pong, Close}` +
  `DeskSessionSender` + `PeerSignalingSender` impl
- `DeskSession::{new, shutdown, handle_message}` (handle_message
  trimmed to the worker-owned `SignalingType` set: terminal,
  manager file/system queries, `EnablePrivateScreen`,
  `UpdateDeskSettings`)
- `should_short_circuit_control` /
  `should_short_circuit_clipboard` (consumed by
  `daemon::pc_manager`)
- `resolve_mdns_host` + `get_mdns_conn` + `MDNS_CONN`
  (consumed by `daemon::pc_manager`)

Worker side (`web/server/src/worker/session.rs`):

- Drop the dead `DeskSessionMessage::WebRTCDropped` arm in
  `desk_rx.recv()`. With the variant gone from `DeskSessionMessage`
  and `start_webrtc` no longer existing to wire the
  `on_peer_connection_state_change` -> `WebRTCDropped` path, this
  arm was unreachable.
- Refresh the heartbeat-task comment that referenced
  `desk_session.rtc_peer_connection_map` (which the field deletion
  also removed).

`cargo fmt -p lcxl-remote-desk-server` reflowed a handful of
pre-existing format drifts in
`daemon/{mod,pc_manager,signaling_proxy,signaling_router}.rs` and
`worker/media_producer.rs`. These are bundled with the A2 commit
(no semantic change; `cargo fmt --all` is part of the local commit
flow per `web/CLAUDE.md`).

## Tests

CLAUDE.md's "code change must add tests" rule is satisfied with two
new round-trip tests for `DeskSessionSender` (the kept-and-still-used
`PeerSignalingSender` implementation), in
`service::signaling::sender_tests`:

- `send_response_round_trips_to_text_signaling_model` — verifies
  outbound `success_response` decodes back to a `SignalingModel`
  whose `signaling_type`, `to_connection_id`, `request_id`, and
  successful `response_state` round-trip cleanly. This is the
  upstream of `worker::session::build_outbound_payload_from_desk_text`'s
  typed-IPC re-classification.
- `send_error_round_trips_with_error_response_state` — verifies
  outbound `send_error` decodes to a `SignalingModel` with a
  non-success `response_state` carrying the given error code +
  message. This is the upstream of
  `WorkerToService::SignalingError` after batch 4 of the typed-IPC
  migration.

The seven `handle_request_control_tests` covering
`should_short_circuit_*` are kept as-is.

Result counts:

- `cargo test -p lcxl-remote-desk-server --lib`: **251 passed**
  (249 → 251, +2 from the new round-trip tests).
- `cargo test -p desk-ipc-protocol --lib`: **42 passed** (unchanged).

## Commits

| Step | Files | Commit |
| --- | --- | --- |
| A1 | `lib.rs`, `daemon/mod.rs` | (web) |
| A2 | `service/signaling.rs` (-1746/+144), `worker/session.rs` (-22/+10), fmt drift in daemon/* + worker/media_producer.rs | (web) |
| (parent) submodule pointer bump | — | (parent workspace) |

## What's still deferred (PR 7 manual e2e)

The eight-item PR 7 sign-off checklist (portable vs daemon-worker
browser e2e, UAC, lock screen, worker crash recovery, multi-browser,
remote management, latency baseline) is still owed and remains a
human-driven validation. Now also covers DeskServer mode (item 2
should be exercised in both ServiceDaemon **and** DeskServer
flavours).
