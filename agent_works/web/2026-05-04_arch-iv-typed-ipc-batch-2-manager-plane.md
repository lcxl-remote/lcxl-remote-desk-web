# Arch IV typed-IPC migration — batch 2: manager plane

## Goal

Migrate the manager-plane signaling group off the
`SignalingMessage` opaque-envelope bridge. This batch covers six
`SignalingType` variants:

| Signaling type | Direction (browser↔worker) | Outcome |
|---|---|---|
| `ManagerSystemInfo` | request `()` / response `SystemInfo` | typed both ways |
| `ManagerSystemStatue` | dead enum (no worker arm, no front-end) | swallow |
| `ManagerFileList` | request `FileListParams` / response `FileListResponse` | typed both ways |
| `ManagerFileDelete` | request `DeleteFileRequest` / response `()` | typed both ways |
| `ManagerQuerySettings` | request `()` / response `RemoteSystemSettings` | typed both ways |
| `ManagerUpdateSettings` | request `RemoteSystemSettings` / response `()` | typed both ways |

The five live request/response pairs each get a dedicated
`ServiceToWorker::Manager*Request` and matching
`WorkerToService::Manager*Response` IPC variant carrying
`request_id`, `connection_id`, and the typed body. `ManagerSystemStatue`
joins the daemon-swallow list for the same reason `ChangeDisplaySettings`
(batch 1) and `AcceptControl` / `DenyControl` (batch 0) did: no
worker handler, no front-end caller.

The migration sticks with the established pattern: the worker still
dispatches into `DeskSession::handle_message` via a typed-aware
`dispatch_typed_signaling_with_request_id` helper rather than
duplicating the manager logic into `worker/`. Responses fall through
the existing `desk_rx` outbound classifier
(`build_outbound_payload_from_desk_text`), which now matches on
`SignalingType` and assembles the typed `Manager*Response` payloads.
The portable / DeskServer code paths still use the legacy
`PeerSignalingSender::send_response` machinery unchanged.

## Scope

### `web/signal-facade/src/model/files.rs`

- `FileListParams` / `FileInfo` / `FileListResponse` gain
  `Clone + Debug` derives. Required so the typed IPC payloads that
  embed them can themselves derive `Clone + Debug` (consistent with
  every other ipc-protocol payload). `FileInfo` already had `Debug`;
  it just needed `Clone`. No wire-format change.

### `web/ipc-protocol/src/message.rs`

- New `ServiceToWorker::Manager*Request` variants (5 of them):
  - `ManagerSystemInfoRequest(ManagerRequestRefPayload)`
  - `ManagerQuerySettingsRequest(ManagerRequestRefPayload)`
  - `ManagerFileListRequest(ManagerFileListRequestPayload)`
  - `ManagerFileDeleteRequest(ManagerFileDeleteRequestPayload)`
  - `ManagerUpdateSettingsRequest(ManagerUpdateSettingsRequestPayload)`
- New `WorkerToService::Manager*Response` variants (5 of them):
  - `ManagerSystemInfoResponse(ManagerSystemInfoResponsePayload)`
  - `ManagerFileListResponse(ManagerFileListResponsePayload)`
  - `ManagerFileDeleteResponse(ManagerResponseRefPayload)` (empty)
  - `ManagerQuerySettingsResponse(ManagerQuerySettingsResponsePayload)`
  - `ManagerUpdateSettingsResponse(ManagerResponseRefPayload)` (empty)
- New payload structs in a new "batch 2" doc-section after the batch
  1 payloads:
  - `ManagerRequestRefPayload { request_id, connection_id }` — shared
    body-less request envelope.
  - `ManagerResponseRefPayload { request_id, connection_id }` —
    shared body-less response envelope (kept distinct from the
    request envelope so daemon-side response code is symmetric with
    request code at the type system).
  - `ManagerFileListRequestPayload { request_id, connection_id, params: FileListParams }`
  - `ManagerFileDeleteRequestPayload { request_id, connection_id, request: DeleteFileRequest }`
  - `ManagerUpdateSettingsRequestPayload { request_id, connection_id, settings: RemoteSystemSettings }`
  - `ManagerSystemInfoResponsePayload { request_id, connection_id, info: SystemInfo }`
  - `ManagerFileListResponsePayload { request_id, connection_id, response: FileListResponse }`
  - `ManagerQuerySettingsResponsePayload { request_id, connection_id, settings: RemoteSystemSettings }`
  - All facade-typed fields use `#[bincode(with_serde)]` so they
    don't need their own `Encode`/`Decode` derives in
    `desk-signal-facade`.
- Five new bincode round-trip tests covering request + response
  envelopes (`manager_request_ref_round_trips_bincode`,
  `manager_file_list_request_round_trips_bincode`,
  `manager_update_settings_request_round_trips_bincode`,
  `manager_response_ref_round_trips_bincode`,
  `manager_system_info_response_round_trips_bincode`).

### `web/server/src/daemon/signaling_router.rs`

- `classify`:
  - `ManagerSystemStatue` moves from worker-owned to a new arm in
    the daemon-owned swallow group (joining `ChangeDisplaySettings`,
    `PrivateScreenStateChanged`, `AudioPlaybackError` from batch 1
    and `AcceptControl` / `DenyControl` from batch 0). Doc-comment
    explains the dead-enum rationale.
  - The remaining manager types stay worker-owned. Comment over
    that arm rewritten: "worker-owned" now means the *handler* runs
    in the worker process; whether the message rides typed IPC or
    the legacy bridge is a routing detail decided in `route` below.
- `route`: 5 new arms for `ManagerSystemInfo`, `ManagerQuerySettings`,
  `ManagerFileList`, `ManagerFileDelete`, `ManagerUpdateSettings` —
  each calls a per-type async helper and returns
  `RouteOutcome::HandledByDaemon`. The legacy bridge no longer
  carries any of these.
- Swallow list extended with `ManagerSystemStatue` (alongside the
  existing daemon-emitted variants).
- 5 new private async helpers
  (`handle_manager_system_info_inbound`, etc.) that share a
  `require_from_connection_id` shape: pull the
  `from_connection_id`, parse the typed body where present, build
  the typed payload, ship via `WorkerManager::send_to_worker`.
  Errors are non-fatal (log + drop) — same fail-soft semantics the
  bridge had for malformed inputs.
- Tests:
  - `classify_daemon_owned_types` extended with `ManagerSystemStatue`.
  - `classify_worker_owned_types` no longer lists it.
  - `route_swallows_daemon_emitted_variants` extended with
    `ManagerSystemStatue`.
  - `route_forwards_unmigrated_worker_owned_variants` rewritten to
    cover the terminal types only (the remaining bridge users).
  - `route_manager_requests_handled_inline_not_bridged` (new) pins
    all five typed manager request paths in one sweep.
  - `route_manager_request_without_connection_id_is_noop` (new)
    pins the malformed-input fail-soft.
  - `route_manager_file_list_with_invalid_payload_is_dropped` (new)
    pins the body-parse fail-soft.

### `web/server/src/daemon/signaling_proxy.rs`

- 5 new arms in the `WorkerToService` match handle the typed
  `Manager*Response` variants. Each calls a new
  `send_manager_response<T>(...)` helper that builds a
  `SignalingType::Manager*` outbound `SignalingModel` via
  `SignalingModel::success_response(request_id, type, None,
  Some(connection_id), data)` and broadcasts the JSON on
  `outbound_tx`. Empty-body responses pass `Option::<&()>::None`.
- New `send_manager_response` private helper that captures the
  build / serialise / broadcast pattern shared across all five
  responses. Build / serialise failures log + drop; non-fatal for
  the message loop.
- New `SignalingType` import (was only `SignalingModel` before
  batch 1; batch 2 now needs the type variant constants too).

### `web/server/src/worker/session.rs`

- 5 new arms in the `ServiceToWorker` match in the IPC main loop
  (just below `ServiceToWorker::UpdateDeskSettings`): each
  `Manager*Request` calls
  `dispatch_typed_signaling_with_request_id(...)` with the original
  `request_id` so the worker's `send_response` echoes it back
  through `desk_rx`.
- New `dispatch_typed_signaling_with_request_id<T>` helper that
  generalises the batch 1 `dispatch_typed_signaling`: the latter
  becomes a thin wrapper that passes `"typed-ipc"` for one-way
  notifications and `Some(data)` for the body. The new function
  takes an explicit `request_id` and an `Option<&T>` body so
  empty-body requests
  (`ManagerSystemInfoRequest` / `ManagerQuerySettingsRequest`) skip
  the synthetic placeholder serialisation.
- `build_outbound_payload_from_desk_text` refactored to delegate
  per-type matching to a new `try_route_typed_outbound` helper
  (the inline `if let` + `matches!` ladder was getting unwieldy).
  - Existing `PrivateScreenStateChanged` arm moved over.
  - 5 new `Manager*` response arms — each pulls
    `to_connection_id` (where `send_response` writes the target
    browser PC id) plus the `request_id` and assembles the typed
    `WorkerToService::Manager*Response` payload.
  - Empty-body responses (`ManagerFileDelete`,
    `ManagerUpdateSettings`) skip the body parse — only
    `to_connection_id` + `request_id` are required.
  - Anything else returns `None`, falling back to the
    `SignalingMessage` bridge.
- 3 new unit tests in `worker::session::tests`:
  - `outbound_dispatch_routes_manager_system_info_response_to_typed_variant`
    pins the body-bearing typed-routing decision on the happy path.
  - `outbound_dispatch_routes_empty_body_manager_responses_to_typed_variants`
    sweeps `ManagerFileDelete` + `ManagerUpdateSettings` empty-body
    responses.
  - `outbound_dispatch_manager_response_without_to_connection_falls_back`
    pins the `to_connection_id`-missing safety net (the helper
    falls back to `SignalingMessage` rather than dropping
    silently).
- New imports: `ManagerFileDeleteRequestPayload`,
  `ManagerFileListRequestPayload`, `ManagerFileListResponsePayload`,
  `ManagerQuerySettingsResponsePayload`, `ManagerRequestRefPayload`,
  `ManagerResponseRefPayload`, `ManagerSystemInfoResponsePayload`,
  `ManagerUpdateSettingsRequestPayload`,
  `DeleteFileRequest`, `FileListParams`, `FileListResponse`,
  `SystemInfo`, `RemoteSystemSettings`.

## Out of scope

- **Terminal management** (`StartTerminal` / `SendDataToTerminal` /
  `ResizeTerminal` / `CloseTerminal` / `ListTerminal` /
  `ReplyFromTerminal` / `TerminalStarted` / `TerminalClosed`) —
  batch 3.
- **Bridge deletion** (`ServiceToWorker::SignalingMessage` /
  `WorkerToService::SignalingMessage` enum variants and the
  `SignalingPayload` struct) — batch 4. Until then they continue
  carrying the unmigrated terminal types.
- **Reading typed manager updates daemon-side** — for example
  `ManagerUpdateSettingsRequest` could in principle let the daemon
  notice a `signaling_url` change and reconnect its upstream WS,
  but the legacy worker handler already persists settings without
  that hook and rewiring is a separate concern.

## Validation

- `cargo test -p desk-ipc-protocol --lib`: **34 passed** / 0 failed
  (was 29; +5 new round-trip tests).
- `cargo test -p lcxl-remote-desk-server --lib`: **242 passed** /
  0 failed (was 236; +6 net new tests — 3 router + 3 worker outbound).
- `cargo check -p desk-signal-facade -p desk-ipc-protocol -p
  lcxl-remote-desk-server --all-targets`: clean (only pre-existing
  warnings — same set as previous batches).
- Pre-existing `desk-capture-engine` `numframestoread <= 0`
  `clippy::absurd_extreme_comparisons` error (documented in earlier
  archives) is unchanged and unrelated.

## Manual e2e

When validating, pull up a connected browser session driving the
manager UI and:

1. **System info query.** Open the host's "system info" page in
   the browser; expect the SystemInfo card to populate. Worker log
   should show `dispatch_typed_signaling_with_request_id` for
   `ManagerSystemInfo`; daemon log should show the matching typed
   `ManagerSystemInfoResponse` arm in `signaling_proxy`.
2. **File browser.** Navigate the file tree, paginate, delete a
   file — the request/response round-trips ride typed IPC. Confirm
   pagination params (`page_no`, `page_count`, filters) survive by
   checking the browser-side network tab matches the request.
3. **Settings save round-trip.** Open the manager settings page,
   change a non-critical knob (e.g. `locale`), save. Worker should
   persist via the existing handler (no behaviour change there);
   the response goes back as typed
   `ManagerUpdateSettingsResponse` / empty body.
4. **Stray `ManagerSystemStatue` inbound.** Manually emit a
   `ManagerSystemStatue` from the front-end devtools (or any
   client). Daemon should log `[router] daemon-emitted variant
   arrived inbound, dropping` at trace level; worker must NOT
   receive anything — i.e. no `UNKNOWN_SIGNALING_TYPE` reply at
   the browser.
