# Arch IV PR 7 — Arch III remnant cleanup

## Goal

Close out the Arch IV migration by deleting the Arch III state-sync IPC,
the daemon's per-connection accept-state cache, and the now-unreachable
worker-side guards / dead IPC types. The `daemon owns PC + SignalingState`
invariant established by PRs 1–6 makes the entire preapproved /
ConnectionAccept / SignalingMessage layer dead weight.

## Scope (what PR 7 deletes)

### IPC protocol (`web/ipc-protocol/src/message.rs`)

- `ServiceToWorker::DesktopSwitching` variant.
- `WorkerToService::DesktopReady`,
  `ConnectionAcceptStateChanged { connection_id, state }`,
  `ConnectionClosed { connection_id }` variants.
- `WorkerInitPayload.preapproved_connections` field.
- `ConnectionAcceptState` struct.
- `ServiceToUI` / `UIToService` enums and their payload types
  (`ServiceStatus`, `ConnectionStatePayload`, `DesktopSwitchPayload`,
  `DesktopSwitchPhase`). Never wired up in this workspace.

`ServiceToWorker::SignalingMessage` /
`WorkerToService::SignalingMessage` / `SignalingPayload` were briefly
removed in cut 1 and **restored in cut 4** — see "Cut 4 (post-audit fix)"
below.

### Daemon (`web/server/src/daemon/`)

- `WorkerManager.active_connections: Arc<Mutex<HashMap<...>>>` cache.
- Helper methods: `track_browser_connection`,
  `update_connection_accept`, `remove_connection`,
  `connection_accept_state`.
- `start_worker(&self, session_id, desktop_name, preapproved)` →
  `start_worker(&self, session_id, desktop_name)`.
- `notify_desktop_switch() -> Vec<(...)>` →
  `notify_desktop_switch() -> ()`. Only the per-PC media-pause
  side-effect remains (the keep-PC pause introduced in PR 6).
- `run_pipe_server` (Windows + Unix paths) drops the `preapproved`
  parameter and the per-connection cache re-seeding block.
- `signaling_proxy.rs` drops the
  `WorkerToService::ConnectionAcceptStateChanged` /
  `ConnectionClosed` / `SignalingMessage` / `DesktopReady` match arms,
  the `track_browser_connection` call, and the worker-forward fallback
  (the router never returns `ForwardToWorker` for any browser-bound type
  in Arch IV; if it ever does the dispatcher logs and drops).

### Worker (`web/server/src/worker/session.rs`)

- `ServiceToWorker::SignalingMessage` and `DesktopSwitching` match arms
  in the IPC main loop.
- `WorkerToService::SignalingMessage` outbound forwarding for
  `DeskSessionMessage::Text` (worker DeskSession in Arch IV produces no
  signaling text the daemon needs).
- `init_payload.preapproved_connections` drain into `DeskSession::new`.
- The `WorkerToService::DesktopReady` test fixture was retargeted to
  `Heartbeat` (DesktopReady no longer exists).

### Worker (`web/server/src/service/signaling.rs`)

- `DeskSession.preapproved` and `daemon_event_tx` fields and their
  `DeskSession::new` parameters.
- `DeskSession::notify_daemon_accept_state` /
  `notify_daemon_connection_closed`.
- `init_ptc_peer_connection`'s preapproved restoration block (the PC
  now always starts with a fresh `SignalingState::default()`).
- `handle_request_control` per-decision daemon notify calls and the
  intermediate `new_state: ConnectionAcceptState` locals.
- The `CloseControl` handler's `notify_daemon_connection_closed` call.
- The `is_session_worker` early-return guards in
  `handle_message` (`RequestRemote` / `Offer` / `Canid` arms). These
  were unreachable after PR 7 cut 1 deleted the IPC arm that fed
  signaling to `handle_message` in SessionWorker mode; the only
  remaining caller is the legacy DeskServer WS path where the guard
  always evaluated false anyway.

## Out of scope

- `service/signaling.rs::should_short_circuit_control` /
  `should_short_circuit_clipboard`: kept in place. They are pure
  one-line helpers that both the legacy `handle_request_control` and
  the new `daemon::pc_manager::handle_require_control` import. Moving
  them to `pc_manager.rs` would create a backwards dependency from the
  legacy module on the new one; they are not Arch III state, just
  shared decision predicates.
- `service/signaling.rs::init_ptc_peer_connection` body and
  `handle_offer` body: kept. They still serve the legacy DeskServer
  startup mode where the desk-server connects directly to a remote
  signaling WS without a daemon. PR 5 only routed portable Default
  mode through Arch IV; DeskServer remains on the legacy path.
- WebRTC PC daemon-to-daemon migration / hot-swap (called out as
  out-of-scope in the original Arch IV plan).

## Cuts as committed

| Cut | Commit | Files | Net diff |
| --- | --- | --- | --- |
| 1 | `be9cc11` | 8 | +78 / −675 |
| 2 | `6d84628` | 2 | +7 / −106 |
| 4 | `e4e01e6` | 3 | +155 / −29 |

(Cut 2 is small because the bulk of the structural delete landed in
Cut 1; Cut 2 is the dead-IPC-types + worker-guard sweep. Cut 3 is the
archive doc itself + the parent-workspace submodule bump and is not
listed here as a code cut. Cut 4 is the post-audit regression fix
described below.)

### Cut 4 (post-audit fix)

Cut 1 deleted `ServiceToWorker::SignalingMessage` and the daemon's
worker-forward fallback on the assumption that every browser-bound
signaling type was already handled daemon-side. **The audit was
incorrect.** `signaling_router::route` still classifies 22 worker-owned
SignalingTypes as `RouteOutcome::ForwardToWorker` because their
handlers live in the user-session worker process, not the daemon:

- `EnablePrivateScreen`, `PrivateScreenStateChanged`,
  `UpdateDeskSettings`, `ChangeDisplaySettings`, `AudioPlaybackError`
- Terminal control: `StartTerminal` / `SendDataToTerminal` /
  `ResizeTerminal` / `CloseTerminal` / `ReplyFromTerminal` /
  `ListTerminal` / `TerminalStarted` / `TerminalClosed`
- Manager queries: `ManagerSystemInfo`, `ManagerSystemStatue`,
  `ManagerFileList`, `ManagerFileDelete`, `ManagerQuerySettings`,
  `ManagerUpdateSettings`
- `AcceptControl`, `DenyControl` reply emission from the legacy
  DeskServer worker path
- `Error`, `Unknown` envelope catch-all

Without the IPC bridge those types silently dropped at the daemon's
proxy — a regression that broke private-screen overlay, terminal,
file management, and runtime settings changes in daemon-worker mode.

Cut 4 restores:
- `ServiceToWorker::SignalingMessage(SignalingPayload)` as a
  *transitional* bridge variant.
- `WorkerToService::SignalingMessage(SignalingPayload)` as the return
  path for worker-emitted replies.
- `SignalingPayload { message, connection_id }` struct.
- `signaling_proxy::handle_inbound_signaling_text` worker forward path
  (drop without `from_connection_id`, send as `SignalingMessage`
  otherwise).
- `signaling_proxy` `WorkerToService::SignalingMessage` arm copying
  the payload onto `outbound_tx`.
- Worker `ServiceToWorker::SignalingMessage` re-parse +
  `DeskSession::handle_message` dispatch.
- Worker `DeskSessionMessage::Text` outbound forwarding as
  `WorkerToService::SignalingMessage`.

The doc-comments on the restored variants mark them transitional —
future work can replace each remaining type with a typed event-
transport variant and finally retire the bridge.

## Validation

- `cargo check -p lcxl-remote-desk-server --all-targets` clean (only
  pre-existing warnings outside PR 7 scope).
- `cargo test -p desk-ipc-protocol`: **23 passed** / 0 failed.
- `cargo test -p lcxl-remote-desk-server --lib`: **221 passed** / 0
  failed (cut 4 added one signaling_proxy regression test).
- `cargo test -p desk-server-version`: **6 passed** / 0 failed.
- Pre-existing `desk-capture-engine`
  `audio_capture::wasapi_capture::tests::test_write_wav`
  abort/panic is unrelated to PR 7 (capture-engine was not modified).

## Manual e2e checklist (deferred to PR 7 sign-off)

These were captured in the original PR 7 plan and are still owed at
sign-off:

1. **portable mode**: `cargo run -p lcxl-remote-desk-server` →
   browser connects → SDP/ICE through in-process IpcTransport →
   picture + control work.
2. **daemon-worker mode**: build + install service → browser connects
   → picture + control work.
3. **UAC**: trigger UAC → browser picture freezes ~1 s then resumes →
   mouse / keyboard click UAC Yes/No.
4. **Lock screen**: Win+L → browser still controls the lock-screen UI.
5. **Worker crash recovery**: manual `kill` of the worker process →
   daemon respawns → browser picture recovers within ~1 s.
6. **Multi-browser**: two browsers connect concurrently → independent
   PCs + independent encoders sharing one capture.
7. **Remote management not regressed**: file list, terminal, system
   info, settings change all flow through `signaling_router` cleanly.
8. **Latency baseline**: portable vs daemon-worker mouse-response
   feel; daemon-worker should be 1–2 ms slower with no perceptible
   difference.

## Notes for the next session

- The Arch IV cutover is structurally complete on this branch
  (`feat/arch-iv-daemon-webrtc`) once PR 7 is merged. There is no PR 8.
- Legacy DeskServer mode still uses the `service/signaling.rs`
  `handle_message` path. If a future cut wants to retire DeskServer
  entirely, that path can also be deleted along with `should_short_*`
  and `init_ptc_peer_connection`.
- The pre-existing capture-engine test panic
  (`audio_capture::wasapi_capture::tests::test_write_wav`,
  `STATUS_STACK_BUFFER_OVERRUN` from a `slice::from_raw_parts`
  precondition) is independent of Arch IV and should be tracked
  separately.
