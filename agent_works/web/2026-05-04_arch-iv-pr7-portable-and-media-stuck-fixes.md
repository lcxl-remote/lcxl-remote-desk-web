# Arch IV PR 7 — Portable-mode swap fix + MediaTransportStuck recovery

## Goal

Address two gaps surfaced during PR 7 manual review:

1. **Portable / Default-mode desktop drift loop**: `signaling_proxy`
   reacted to `WorkerToService::DesktopChanged` by unconditionally
   calling `WorkerManager::start_worker(...)`, which spawns an external
   session-worker via `CreateProcessAsUserW`. In portable mode the
   worker is an in-process task, single-process can't cross window
   stations, and the entry point isn't running under SYSTEM — so the
   external launch path either fails outright or half-succeeds in
   confusing ways. Under UAC / Win+L the in-process pipeline could
   silently veer off the keep-PC happy path.
2. **`MediaTransportStuck` not auto-recovered**: the worker emits
   `WorkerToService::Error { code: -1001, ... }` when an I-frame send
   times out on the media transport, but the daemon side only logged
   the error. The Arch IV plan called for the daemon to issue
   `StopMedia` + `StartMedia` + `ForceKeyframe` for the affected
   connection; without that, a stuck stream stayed stuck.

## Scope

### `web/ipc-protocol/src/message.rs`

- Added `connection_id: Option<String>` to `ErrorPayload` (with
  `#[serde(default)]` so legacy JSON-only emitters still decode).
  Bincode is positional and daemon/worker bump together in this
  cutover, so the new field doesn't break the wire format on
  matched-version pairs.
- Promoted `pub const ERROR_CODE_MEDIA_TRANSPORT_STUCK: i32 = -1001`
  into the protocol crate so the daemon can match on it without
  importing from `worker::media_producer`.
- New tests: `error_payload_connection_id_round_trips_bincode`,
  `error_payload_accepts_legacy_json_without_connection_id`,
  `media_transport_stuck_error_code_is_stable` (relocated from
  `media_producer`).

### `web/server/src/worker/media_producer.rs`

- Replaced local `ERROR_CODE_MEDIA_TRANSPORT_STUCK` with the
  re-export from `desk-ipc-protocol`.
- I-frame timeout emission now sets
  `connection_id: Some(connection_id.to_string())` so the daemon can
  scope the recovery to the affected PC instead of having to parse
  the human-readable `message` field.
- Dropped the now-redundant local stability test for the constant.

### `web/server/src/worker/session.rs`

- Existing `ErrorPayload` construction (init-time config parse
  failure) updated with explicit `connection_id: None`.

### `web/server/src/daemon/worker_manager.rs`

- New `is_inprocess: Arc<AtomicBool>` field on `WorkerManager`,
  defaults to `false`, latched to `true` at the head of
  `start_inprocess_worker`. One-way switch — supported topologies
  don't change at runtime.
- New `pub fn is_inprocess(&self) -> bool` getter.
- `handle_crash_recovery` short-circuits when in-process mode: there
  is no external process to relaunch, and `start_worker` would
  attempt `CreateProcessAsUserW` from a non-SYSTEM context. Logs
  the skip and returns.
- New tests: `is_inprocess_false_by_default`,
  `handle_crash_recovery_is_noop_when_inprocess`.

### `web/server/src/daemon/signaling_proxy.rs`

- `WorkerToService::DesktopChanged` arm checks
  `worker_mgr.is_inprocess()` before scheduling the swap. In-process
  mode logs at `debug!` and continues — desktop_monitor still fires
  in portable workers because they don't know their topology, so
  swallowing the event in the proxy is the correct reactive boundary.
- `WorkerToService::Error` arm dispatches
  `ERROR_CODE_MEDIA_TRANSPORT_STUCK` errors to a new
  `PcRegistry::reset_media_for(connection_id, &worker_mgr)` task. The
  log line also surfaces `connection_id` for telemetry. Errors with a
  matching code but no `connection_id` log a warning and fall through
  (the worker should always populate it for this code; the warning is
  a regression guard).

### `web/server/src/daemon/pc_manager.rs`

- New `PcRegistry::reset_media_for(connection_id, &worker_mgr)`:
  pauses the PC's media ingestion, sends `StopMedia`, then re-issues
  the cached `StartMediaPayload` + `ForceKeyframe`. PCs without a
  cached offer (the stuck error fired before the first Offer ever
  landed) get only the pause + StopMedia; the doc-comment notes the
  caller would need to redo `handle_offer` in that edge case.
- Added `StopMediaPayload` to the existing `desk_ipc_protocol::message`
  use-list.
- New tests: `reset_media_for_unknown_connection_is_noop`,
  `reset_media_for_pauses_pc_even_without_cached_offer`.

## Out of scope

- Did **not** suppress `desktop_monitor` spawning in portable
  workers. The worker doesn't know its own topology and adding a
  topology hint to `WorkerInitPayload` would expand the scope; the
  daemon-side gate is sufficient (and arguably more correct, since
  the proxy is the right place to decide what to do with an event).
- Did **not** restructure `signaling_proxy::run_signaling_proxy` to
  expose the per-message handler as a unit-testable helper. The
  underlying primitives (`is_inprocess()`, `reset_media_for`) have
  unit tests; the integration is small and gets covered by the
  existing manual e2e plan.
- `UpdateMediaSettings` runtime apply (still ack-only) — known TODO
  carried in `media_producer::update_settings`; tracked separately.

## Validation

- `cargo test -p desk-ipc-protocol`: **26 passed** / 0 failed
  (was 23 — added 3 new tests).
- `cargo test -p lcxl-remote-desk-server --lib`: **224 passed** / 0
  failed (was 221 — added 3 new tests, removed 1 redundant constant
  test that moved to `desk-ipc-protocol`).
- `cargo check -p lcxl-remote-desk-server -p desk-ipc-protocol
  --all-targets`: clean (only pre-existing warnings).
- `cargo clippy` on the modified packages: clean (no new lints).
- Pre-existing `desk-capture-engine`
  `audio_capture::wasapi_capture::tests::test_write_wav` panic and
  the `numframestoread <= 0` clippy::absurd_extreme_comparisons in
  the same crate are unchanged and unrelated.

## Manual e2e re-validation owed

Same checklist as the parent PR 7 archive — items 3 (UAC), 4 (lock
screen) and 5 (worker crash recovery) now have a meaningfully
different code path in portable mode and should be exercised in
both portable and daemon-worker topologies.
