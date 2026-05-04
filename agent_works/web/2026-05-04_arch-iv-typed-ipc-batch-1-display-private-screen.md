# Arch IV typed-IPC migration — batch 1: display / private-screen

## Goal

Migrate the first business-domain group off the transitional
`ServiceToWorker::SignalingMessage` / `WorkerToService::
SignalingMessage` opaque-envelope bridge. This batch covers the five
display / private-screen / desk-settings signaling types:

| Signaling type | Direction | Outcome |
|---|---|---|
| `EnablePrivateScreen` | browser → worker | typed `ServiceToWorker::EnablePrivateScreen` |
| `UpdateDeskSettings` | browser → worker | typed `ServiceToWorker::UpdateDeskSettings` (plus the existing media-knob fan-out via `UpdateMediaSettings`) |
| `PrivateScreenStateChanged` | worker → browser | typed `WorkerToService::PrivateScreenStateChanged` |
| `ChangeDisplaySettings` | dead enum | swallow (front-end never emits, worker has no handler) |
| `AudioPlaybackError` | dead in daemon-worker mode | swallow (only `service::signaling`'s portable PC ever produces it; daemon's `pc_manager` does not attach an `on_track` handler today) |

Approach: type-safety up to the IPC boundary; the worker still
dispatches into the existing `DeskSession::handle_message` arms via a
thin `dispatch_typed_signaling` helper rather than duplicating the
private-screen / settings logic into `worker/`. That keeps the
portable / DeskServer code paths working unchanged. Batches 2–4 will
follow the same shape (manager plane, terminal, then bridge
deletion).

## Scope

### `web/ipc-protocol/Cargo.toml`

- Adds `desk-signal-facade = { workspace = true }` so the IPC layer
  can carry typed `DeskSettings` / `PrivateScreenStateChangedData`
  fields (chosen "method A" of the design discussion, per user). The
  facade crate is leaf-ish (does not depend on the server) — no
  cycle. Heavy transitive deps (actix-web / webrtc / utoipa) are
  not used by the IPC types and only show up in their own
  compilation units.
- Switches `bincode = { workspace = true }` to enable the `serde`
  feature so `#[bincode(with_serde)]` compiles. Required to ride
  `DeskSettings` and `PrivateScreenStateChangedData` over the wire
  without giving them their own bincode `Encode`/`Decode` derives in
  the facade crate.

### `web/ipc-protocol/src/message.rs`

- New `ServiceToWorker::EnablePrivateScreen(EnablePrivateScreenPayload)`,
  `ServiceToWorker::UpdateDeskSettings(UpdateDeskSettingsPayload)`,
  `WorkerToService::PrivateScreenStateChanged(PrivateScreenStateChangedPayload)`
  variants under a new "Arch IV typed-IPC migration batch 1"
  doc-section per enum.
- New payload structs:
  - `EnablePrivateScreenPayload { connection_id, enable }`
  - `UpdateDeskSettingsPayload { connection_id, settings: DeskSettings }`
    — uses `#[bincode(with_serde)]` on the `settings` field.
  - `PrivateScreenStateChangedPayload { connection_id, data: PrivateScreenStateChangedData }`
    — same `with_serde` treatment on `data`.
- Three new bincode round-trip tests:
  - `enable_private_screen_round_trips_bincode` covers both
    `enable = true` / `false` so a field reorder would flip the
    semantics on matched-version pairs.
  - `update_desk_settings_round_trips_bincode` exercises the
    `#[bincode(with_serde)]` path for `DeskSettings` with non-default
    `video_fps` / `video_quality` / `wayland_control_mode` so a
    schema change to the facade `DeskSettings` shows up here.
  - `private_screen_state_changed_round_trips_bincode` round-trips
    the reverse path with `is_supported = false` + an `error_msg`.

### `web/server/src/daemon/signaling_router.rs`

- `classify`:
  - `ChangeDisplaySettings`, `PrivateScreenStateChanged`,
    `AudioPlaybackError` move from worker-owned to a new
    daemon-owned arm with a per-variant rationale doc-comment
    (dead enum / worker→browser only / dead in daemon-worker mode).
  - `EnablePrivateScreen` / `UpdateDeskSettings` stay in the
    worker-owned arm but the `route` dispatch now ships them on
    typed IPC inline; the comment over the arm spells out that the
    legacy `SignalingMessage` opaque envelope no longer carries
    these two.
- `route`:
  - Swallow list extended with `ChangeDisplaySettings`,
    `PrivateScreenStateChanged`, `AudioPlaybackError` (joining
    `Answer` / `Init` / `AcceptControl` / `DenyControl` /
    `DesktopSwitching` / etc.). Trace-level log; behaves identically
    to the existing daemon-emitted swallow.
  - New explicit arm for `SignalingType::EnablePrivateScreen` →
    `handle_enable_private_screen_inbound` helper.
  - `UpdateDeskSettings` arm refactored: was returning
    `RouteOutcome::ForwardToWorker` and only sniffing media knobs;
    now returns `HandledByDaemon` and ships both the typed
    `UpdateMediaSettings` (existing fan-out) AND the typed
    `UpdateDeskSettings` to the worker. The legacy bridge no longer
    carries this type.
- Two new private async helpers:
  - `handle_enable_private_screen_inbound`: parses
    `EnablePrivateScreenData` from the inbound `SignalingModel`,
    requires a `from_connection_id`, sends typed
    `ServiceToWorker::EnablePrivateScreen` via
    `WorkerManager::send_to_worker`. Parse / send failures log + drop
    (no fail-the-WS).
  - `handle_update_desk_settings_inbound`: parses `DeskSettings`,
    fans out the existing media broadcast, then ships typed
    `ServiceToWorker::UpdateDeskSettings` to the worker. Missing
    `from_connection_id` is tolerated (server-wide settings ship
    with `connection_id = "<unscoped>"`).
- Tests updated:
  - `classify_daemon_owned_types` extended with the three swallowed
    variants; `classify_worker_owned_types` no longer lists them.
  - `route_swallows_daemon_emitted_variants` extended (and
    doc-comment updated to spell out the per-variant reason).
  - Removed `route_forwards_worker_owned_variants`'s `EnablePrivateScreen`
    expectation; replaced with two new tests:
    - `route_enable_private_screen_handled_inline_not_bridged`
      pins the typed dispatch — `route` must return
      `HandledByDaemon` (no SignalingMessage bridge fallback).
    - `route_enable_private_screen_without_connection_id_is_noop`
      pins the malformed-input fail-soft path.
  - `route_update_desk_settings_forwards_and_broadcasts` renamed to
    `route_update_desk_settings_handled_inline_not_bridged` with the
    same fan-out assertion plus a `HandledByDaemon` outcome.
  - `route_update_desk_settings_with_invalid_payload_still_forwards`:
    expectation flipped to `HandledByDaemon` (the bridge fallback no
    longer exists for this type).

### `web/server/src/daemon/signaling_proxy.rs`

- New arm in the `WorkerToService` match that handles
  `PrivateScreenStateChanged`. Builds a
  `SignalingType::PrivateScreenStateChanged` `SignalingModel` via
  `SignalingModel::new_request(..., Some(connection_id), Some(&data))`
  — the same wire shape browsers already see today — and broadcasts
  it on the existing `outbound_tx` so every connected WS sink ships
  it back. Build / serialise failures log + drop (non-fatal for the
  proxy).
- New imports: `SignalingType` (was only `SignalingModel` before).

### `web/server/src/worker/session.rs`

- New imports: `EnablePrivateScreenPayload`,
  `UpdateDeskSettingsPayload`,
  `PrivateScreenStateChangedPayload`,
  `EnablePrivateScreenData`, `PrivateScreenStateChangedData`,
  `SignalingType`.
- New `dispatch_typed_signaling<T>` helper that converts a typed
  `ServiceToWorker` payload back into a `SignalingModel` and feeds
  it to `desk_session.handle_message`. Carries an in-line
  doc-comment explaining the rationale (avoids duplicating the
  EnablePrivateScreen / UpdateDeskSettings logic that the portable
  / DeskServer paths still need; subsequent batches will retire the
  helper as `handle_message` shrinks).
- Two new arms in the `ServiceToWorker` match in the IPC main loop
  (just below `WhiteboardCommand`):
  - `EnablePrivateScreen(payload)` → `dispatch_typed_signaling(...,
    SignalingType::EnablePrivateScreen, connection_id,
    &EnablePrivateScreenData { enable })`.
  - `UpdateDeskSettings(payload)` → `dispatch_typed_signaling(...,
    SignalingType::UpdateDeskSettings, connection_id, &settings)`.
- Reverse path: the `desk_rx` arm (`DeskSessionMessage::Text`) used
  to wrap every text blob into a `WorkerToService::SignalingMessage`.
  Now goes through a new `build_outbound_payload_from_desk_text`
  classifier:
  - For `SignalingType::PrivateScreenStateChanged` (with a parseable
    `PrivateScreenStateChangedData` and a `to_connection_id`),
    ship typed `WorkerToService::PrivateScreenStateChanged`.
  - Everything else falls back to `SignalingMessage` (same
    bridge behaviour batches 2–4 will progressively remove).
  - Malformed JSON: also `SignalingMessage` fallback (preserves the
    fail-soft logging behaviour the bridge had).
  - Note on the `to_connection_id` choice: the worker constructs its
    PrivateScreenStateChanged model via `SignalingModel::new_request(
    ..., Some(connection_id), Some(&data))` — `new_request` puts that
    arg in `to_connection_id` (server-initiated request to a
    specific browser), not `from_connection_id`. The first attempt
    of this helper read the wrong field and the unit test caught it.
- Three new unit tests in `worker::session::tests`:
  - `outbound_dispatch_routes_private_screen_state_changed_to_typed_variant`
    pins the typed-routing decision.
  - `outbound_dispatch_falls_back_to_signaling_message_for_unmigrated_types`
    proves terminal / manager / etc. still ride the bridge.
  - `outbound_dispatch_falls_back_when_payload_is_not_signaling_model`
    pins the malformed-JSON fail-soft.

## Out of scope

- **`AudioPlaybackError` typed migration** — the daemon's
  `pc_manager` does not attach an `on_track` handler today, so the
  variant is dead in daemon-worker mode. A separate PR needs to
  (a) attach `on_track` in `pc_manager`, and (b) ship typed
  `WorkerToService::AudioPlaybackError` from there. Tracking note
  carried in the `classify` doc-comment.
- **`ChangeDisplaySettings` implementation** — front-end never
  emits it and the worker has no handler. Treating it as a dead
  enum + swallow matches what AcceptControl / DenyControl got in
  batch 0; resurrecting the feature would be a separate user-story.
- **Manager plane / terminal typed migration** — batches 2 and 3.
- **Deleting the `SignalingMessage` opaque-envelope variants** —
  batch 4. Until then they continue carrying the unmigrated 14
  worker-owned `SignalingType`s.

## Validation

- `cargo test -p desk-ipc-protocol --lib`: **29 passed** / 0 failed
  (was 26; +3 new round-trip tests).
- `cargo test -p lcxl-remote-desk-server --lib`: **236 passed** /
  0 failed (was 231; +5 net new tests after rename consolidations).
- `cargo check -p desk-ipc-protocol -p lcxl-remote-desk-server
  --all-targets`: clean (only pre-existing warnings — same set as
  the previous PR 7 / batch 0 cuts).
- Pre-existing `desk-capture-engine` `numframestoread <= 0`
  `clippy::absurd_extreme_comparisons` error (documented in earlier
  archives) is unchanged and unrelated.

## Manual e2e

When validating, pull up a connected browser session and:

1. **Private-screen toggle.** Click the private-screen
   enable/disable button. Expect: worker log shows the typed IPC
   arm logging `dispatch_typed_signaling` and the existing
   `DeskSession::handle_message` arm running unchanged. Tauri
   shell visibility flips. State broadcast arrives back at the
   browser (look for `[SignalingProxy] PrivateScreenStateChanged`
   or the inverse — the daemon now constructs the outbound
   SignalingModel itself; logs may be quieter).
2. **fps / quality slider.** Same as the previous
   `UpdateMediaSettings` live-apply test (encoder retunes within
   one frame interval); the difference for batch 1 is that the
   non-media `wayland_control_mode` / `private_screen` fields now
   reach the worker via typed IPC instead of the
   `SignalingMessage` bridge — confirm by checking the worker log
   for the typed dispatch path rather than the legacy
   `SignalingMessage` parse line.
3. **Stray protocol-error inputs.** Manually emit a
   `ChangeDisplaySettings` or `PrivateScreenStateChanged` from
   the browser side (e.g. via the React dev console) — daemon
   should log `[router] daemon-emitted variant arrived inbound,
   dropping` at trace level and the worker must NOT receive
   anything (no `UNKNOWN_SIGNALING_TYPE` reply at the browser).
