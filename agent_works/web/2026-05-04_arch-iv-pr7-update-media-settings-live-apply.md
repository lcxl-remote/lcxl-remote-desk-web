# Arch IV PR 7 — UpdateMediaSettings live-apply

## Goal

Make `ServiceToWorker::UpdateMediaSettings` actually retune the live
worker pipeline instead of being an ack-only stub. Per the post-PR 7
audit (item 5), the IPC variant existed and the worker IPC main loop
already routed it to `MediaProducer::update_settings`, but two pieces
were missing:

1. **No daemon-side trigger** — nobody sent the variant. UI changes
   to fps / quality flow as `SignalingType::UpdateDeskSettings`
   through the SignalingMessage bridge, where the worker's legacy
   `DeskSession::handle_update_desk_settings` writes them to a
   `tokio::sync::watch` channel — but the only subscriber was the
   *legacy* `service::signaling::capture_screen_task`, which Arch IV
   replaced with `MediaProducer`. The Arch IV pipeline never saw the
   change.
2. **Worker stub** — `MediaProducer::update_settings` only
   `warn!`-logged; the per-connection encoder + ticker stayed pinned
   to whatever values the original `StartMedia` brought in.

## Scope

### `web/server/src/worker/media_producer.rs`

- `ConnectionTask` gains `settings_tx: mpsc::UnboundedSender<
  UpdateMediaSettingsPayload>`. `start_media` creates the channel
  and hands the receiver to the spawned video pipeline thread.
- `update_settings(payload)` now routes the payload through the
  per-connection sender. Unknown connection_id → silent debug log
  (the daemon may race a `StopMedia`).
- New helper `drain_settings_updates(...)` that the video loop calls
  on each tick before honouring the keyframe flag. It drains every
  pending payload via `try_recv`, applies fps / quality changes to
  the live `merged_settings`, rebuilds the `tokio::time::Interval`
  on fps changes (intervals can't be retuned in place), and returns
  whether anything actually changed.
- The video loop reacts to a true return by recreating the encoder
  via the existing `create_video_encoder(&merged_settings, ...)`
  path and flips `next_pass_is_idr = true` so the first frame after
  the swap is an IDR (browsers re-decode cleanly).
- `bitrate_kbps` is wired through the IPC + drain path but currently
  emits a debug-log breadcrumb instead of being applied — per-codec
  mapping (h264 bps vs vpx bps vs av1 quality-only) lives in the
  `DeskSettings` codec-specific structs and a runtime override path
  needs a follow-up cut. Driving `quality` retunes the auto-bps
  branch in `DeskSettings::get_*_encoder_settings` already.
- Audio pipeline is intentionally not subscribed — Opus owns its
  own frame size (20 ms fixed) and bitrate is set at construction;
  runtime audio retuning needs a separate variant once a UI
  surface exists for it.

### `web/server/src/daemon/pc_manager.rs`

- New `PcRegistry::broadcast_media_settings_update(worker_mgr,
  fps, bitrate_kbps, quality)`: iterates every PC that has a
  cached `StartMediaPayload` (i.e. has negotiated an offer) and
  emits `ServiceToWorker::UpdateMediaSettings` per connection.
  All-`None` payloads short-circuit so unrelated `UpdateDeskSettings`
  messages don't fan out a no-op IPC.

### `web/server/src/daemon/signaling_router.rs`

- New explicit arm for `SignalingType::UpdateDeskSettings`: parses
  the inbound payload as `DeskSettings`, calls
  `broadcast_media_settings_update` with `fps = video_fps,
  quality = video_quality, bitrate_kbps = None`, and still returns
  `RouteOutcome::ForwardToWorker` so the worker's existing handler
  applies non-media fields (`wayland_control_mode`, private_screen
  flags, etc.). Parse failures log and fall through to forward-only
  so the worker still gets a chance to log its own validation
  error.

### Tests

- `media_producer::drain_settings_updates_applies_fps_and_quality`:
  applies fps + quality to `merged_settings`, recomputes
  `frame_duration_ns`, returns `true`. Repeat with same values is
  a no-op (returns `false`).
- `media_producer::drain_settings_updates_ignores_fps_zero_and_bitrate`:
  pins the `fps = 0` sentinel-skip and the bitrate not-yet-applied
  behaviour so a future schema change shows up as a test failure.
- `pc_manager::broadcast_media_settings_update_all_none_is_noop`:
  short-circuit when no knob is set.
- `pc_manager::broadcast_media_settings_update_skips_pcs_without_cached_offer`:
  iterating PCs without a cached payload completes cleanly without
  panicking or synthesizing a default StartMedia.
- `signaling_router::route_update_desk_settings_forwards_and_broadcasts`:
  valid DeskSettings JSON returns `ForwardToWorker` (no panic over
  empty registry).
- `signaling_router::route_update_desk_settings_with_invalid_payload_still_forwards`:
  malformed payload still bridges so the worker logs the validation
  error in its own context.

## Out of scope

- **bitrate_kbps live-apply** — wired through the IPC + drain code
  paths but not yet applied (per-codec routing). Documented as a
  TODO breadcrumb in `drain_settings_updates`. UI today only
  surfaces a quality slider, which the recreate-encoder path
  already honours via `DeskSettings::get_*_encoder_settings`.
- **Audio settings live-apply** — see scope note above.
- **Codec swap** — not in scope, requires SDP renegotiation.
  `UpdateMediaSettingsPayload` deliberately omits a codec field.
- **Coalescing** — `try_recv` drains every pending payload on each
  tick and applies them in order; the encoder rebuild fires once
  per drain so a burst of UI changes converges to a single rebuild
  on the next frame interval. Fine for operator-scale churn; not
  meant for sub-frame retuning.

## Validation

- `cargo test -p lcxl-remote-desk-server --lib`: **230 passed** /
  0 failed (was 224 before — added 6 new tests).
- `cargo test -p desk-ipc-protocol`: 26 passed (unchanged).
- `cargo check -p lcxl-remote-desk-server -p desk-ipc-protocol
  --all-targets`: clean (only pre-existing warnings from
  capture-engine, input-injection, signal).
- `cargo clippy` on the modified packages: no new lints on
  changed files.

## Manual e2e

When validating, pull up a connected browser session and:

1. Move the **fps slider** in the desk settings UI. Expect: brief
   freeze + IDR within one frame interval, then steady stream at
   the new fps. Check the worker log for
   `[MediaProducer] UpdateMediaSettings queued` and
   `[MediaProducer] Live settings changed; recreating encoder`
   lines.
2. Move the **quality slider** at the same fps. Expect: IDR burst
   on the next encode pass, encoder visibly produces different
   bitrate (compare via Chrome `chrome://webrtc-internals` or
   `getStats()`).
3. Settings unrelated to media (wayland_control_mode,
   private_screen_settings, ...) — confirm the SignalingMessage
   bridge still applies them on the worker side
   (`DeskSession::handle_update_desk_settings`).
4. Disconnect the browser, change settings while disconnected,
   then reconnect — first `StartMedia` after reconnect should
   pick up the new defaults from cached `DeskSettings` (no live
   IPC needed). This is the non-regressed "first connect" path.
