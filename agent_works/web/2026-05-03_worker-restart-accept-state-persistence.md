# Worker-restart accept-state persistence (Arch III)

Date: 2026-05-03
Branches: web `fix/service-mode-dtls-rustls-provider` / parent `feat/service-worker-arch-2`

## Context

Continuation of the service-mode hardening track started in
`2026-05-02_service-mode-hardening-and-uac-detection.md`. After Phase 1
(SYSTEM-token Winlogon launch, lock-screen worker freeze fix, heartbeat
watchdog, dialog-flash fix attempt) the remote-control flow had three
remaining problems that surfaced sequentially:

1. **Stale frontend bundle** — the `setInitData(null)` desktop-switch fix
   from the prior session never reached the browser because the debug
   build scripts only `npm ci`'d but never `npm run build`'d.
2. **Approval dialog hung when Tauri was offline** — when the daemon's
   Aggregator received a `SecurityApprovalRequest` from a worker forwarder
   and zero Tauri clients were subscribed, the broadcast was silently
   dropped, the worker's `request_approval` oneshot waited forever, and
   the heartbeat watchdog killed the worker after 30 s.
3. **Watchdog killed worker even when Tauri was online** — if the user
   simply did not click the dialog within 30 s, the worker's main
   `tokio::select!` was parked on the inline `await` of
   `check_security_permission`, so the heartbeat-timer arm of the same
   select never got polled. No IPC traffic → watchdog kill.

Resolving 1–3 surfaced the deeper structural problem the user wanted
analysed:

4. **Mouse / keyboard dead after UAC** — after a UAC desktop switch (or
   any worker restart) the new worker's `SignalingState::accept_control`
   defaults to `false`, so even though the browser still believes it has
   control, all input events are silently dropped at the worker. Tauri
   re-prompting is not viable: during UAC the active input desktop is
   the secure desktop (Winlogon) and Tauri shells only render on
   `WinSta0\Default`.

This document covers the fixes for 1–3 plus the architectural analysis
that produced **Arch III** (the chosen fix for 4) and its implementation.

## Fixes shipped sequentially

### A. `build_debug.{ps1,sh}` actually run `npm run build`

Web commit `f70c912` `fix(build): run npm run build and use absolute dist
path`. Before: scripts only ran `npm ci` (install deps) — `vite-project/dist/`
was never regenerated. The `.sh` variant additionally had a relative-path
bug (`cp -r dist/*` after `popd` → wrong cwd). Both fixed.

### B. Aggregator denies approval immediately when no Tauri client

Web commit `aebfbd4` `fix(host_control): deny approval immediately when no
Tauri client`. Added `HostControlHub::handle_upstream_approval_request` —
when the Aggregator receives a `SecurityApprovalRequest` from a worker
forwarder, it checks `has_tauri_ui()` first. If zero Tauri clients, it
immediately routes back `SecurityApprovalSubmit { approved: false,
remember: false }` to the originating forwarder via `route_to_forwarder`.

The 3-state model (`Some(true)` direct allow / `Some(false)` direct deny /
`None` requires approval) in `check_security_permission` is unchanged.
This fix only affects the `None` branch when the hub's UI is unreachable.

Tests: 2 new (deny-without-tauri, broadcast-when-tauri-present). 57/57 in
`host_control::` pass.

### C. Decouple worker heartbeat from main `select!` loop

Web commit `85a7998` `fix(worker): keep heartbeats flowing during long-
running handlers`. The worker's main loop awaited `handle_message` inline
inside one branch of `tokio::select!`, which parked all other branches —
including the heartbeat-timer one — until the handler returned.

Refactor:
- `spawn_ipc_writer_task(writer, mpsc_rx)` owns the IPC writer and drains
  an unbounded mpsc.
- `spawn_heartbeat_task(writer_tx, interval)` pushes `Heartbeat` into the
  mpsc on its own cadence regardless of what the main loop is doing.
- Main loop sends outbound payloads via `writer_tx.send(...)` instead of
  writing to `writer` directly; `desk_rx` and `desktop_change_rx` arms
  push to the same mpsc.
- Shutdown order: abort heartbeat task, drop `writer_tx`, await writer
  task — so the writer drains cleanly and the heartbeat task can't keep
  producing into a dying queue.

`active_connections` in the dedicated heartbeat path is hard-coded to 0
since the count lives in the main loop and the daemon only logs it at
trace level.

Tests: heartbeat task emits on configured interval and exits when queue
closes; writer task drains queued messages in order and exits when all
senders are dropped.

## Architectural analysis (multi-AI review)

The user reported: *"接受控制下执行 UAC 后浏览器鼠标点击等行为都不能动了"*. Initial
thought was Option A (frontend re-sends `RequireControl` after auto-
reconnect). User rejected: *"没有勾'记住'，那么 tauri 弹框的是用户的桌面上，UAC
的桌面是看不到 tauri 的弹框的"* — the secure desktop hides any Tauri
prompt, so re-prompting during UAC is structurally broken.

### Multi-session option matrix

Four architectures were enumerated, with multi-session considerations:

| Arch | PC unbroken | No flicker | No re-prompt | Effort | Multi-session |
|---|---|---|---|---|---|
| **I** daemon owns WebRTC | ✅ | ✅ | ✅ | very large | trivially supports |
| **II** single SYSTEM worker switches desktop | ✅ | ✅ | ✅ | medium (POC needed) | only intra-session; falls back to III for session switch |
| **III** daemon caches state, worker still restarts | ❌ (1-2 s blackout) | ❌ | ✅ | small | natively supports |
| **IV** Arch II's incremental form | ✅ | ✅ | ✅ | medium | same as II |

**Multi-session note**: Windows can run multiple OS sessions concurrently
(Fast User Switching, RDP + console, Server SKUs). DXGI/audio/GDI
resources are per-session, so console-session switches always require
worker recreation regardless of the chosen architecture for UAC. **Arch
III is therefore the unavoidable foundation** — even if Arch II is later
adopted for the intra-session UAC case, III is needed for cross-session
switches and crash recovery.

### Reviewer feedback incorporated

Two rounds of external AI review surfaced three high/medium-risk issues
in the original Arch III draft:

1. **Don't infer clipboard approval from browser intent** — original
   draft proposed parsing worker→browser `AcceptControl` signaling and
   pulling `accept_clipboard_sync` from the most recent `RequireControl`.
   This is the browser's *request*, not the worker's *decision*; if the
   user denied clipboard, the daemon would still record `true` →
   privilege escalation on next restart. **Fix**: drop signaling
   parsing entirely; add an explicit `WorkerToService::ConnectionAccept-
   StateChanged { connection_id, state }` IPC variant that the worker
   emits after every authoritative `SignalingState` mutation. Worker is
   the source of truth.

2. **Short-circuit must be narrowly scoped** — original draft said "if
   `signaling_state.accept_control == true` already, skip security
   check". This breaks the release path: `RequireControl` with `accept ==
   false` would silently no-op and `CloseControl` would never clear
   state. **Fix**: bypass the security check **only** when both
   `control_data.accept == true` AND `signaling_state.accept_control ==
   true`. Same pattern for clipboard, treated as an independent
   permission (never auto-approved from control approval).

3. **Memory leak risk** — original draft removed map entries only on
   desktop switch (drain), not on browser disconnect. Long-running
   daemon → unbounded growth. **Fix**: add
   `WorkerToService::ConnectionClosed { connection_id }`; emit from the
   worker's `WebRTCDropped` arm and from the `CloseControl` PC-removal
   site; daemon's `worker_rx` loop calls `remove_connection`.

4. **Mutex choice** — clarified that `active_connections` uses
   `std::sync::Mutex` (no `.await` inside any critical section); the old
   `tokio::sync::Mutex` for the previous `active_browser_ids` was
   overkill and risked accidental async-holding.

## Final Arch III implementation

### Approach

Daemon caches per-connection `ConnectionAcceptState` (`accept_control`,
`accept_clipboard_sync`). Worker is authoritative — it emits state to
the daemon via dedicated IPC variants after every `SignalingState`
mutation. On worker (re)start the daemon ships the cached map into
`WorkerInitPayload.preapproved_connections`; the new worker pre-populates
`SignalingState` at PC-creation time and proactively re-sends
`AcceptControl` to the browser.

### Files changed

| File | Change |
|---|---|
| `web/ipc-protocol/src/message.rs` | New `ConnectionAcceptState` struct (`Copy + Default + Serialize + Deserialize`). New `WorkerToService::ConnectionAcceptStateChanged { id, state }` and `ConnectionClosed { id }` variants. New `WorkerInitPayload.preapproved_connections: Vec<(String, ConnectionAcceptState)>` with `#[serde(default)]` for back-compat. |
| `web/server/src/daemon/worker_manager.rs` | Replaced `active_browser_ids: HashSet<String>` with `active_connections: HashMap<String, ConnectionAcceptState>` using `std::sync::Mutex`. New methods `update_connection_accept(id, state)`, `remove_connection(id)`, idempotent `track_browser_connection`. `notify_desktop_switch` returns `Vec<(String, ConnectionAcceptState)>`. `start_worker` and `run_pipe_server` accept `Vec<(String, ConnectionAcceptState)>` and feed it into `WorkerInitPayload`. Also re-seeds `active_connections` from `preapproved` before sending `Init` so a quick desktop re-switch right after restart still ships state forward. |
| `web/server/src/daemon/signaling_proxy.rs` | New match arms in `worker_rx.recv()` loop: `ConnectionAcceptStateChanged → update_connection_accept`, `ConnectionClosed → remove_connection`. No signaling parsing for state — the worker is authoritative. |
| `web/server/src/daemon/session_monitor.rs` | Updated `start_worker` call site for the new tuple type. |
| `web/server/src/worker/session.rs` | Created `writer_tx` BEFORE `DeskSession::new` so the session can hold a clone for IPC emissions. Plumbed `init_payload.preapproved_connections` into `DeskSession`. The `WebRTCDropped` arm emits `ConnectionClosed`. |
| `web/server/src/service/signaling.rs` | `DeskSession` gained two fields (`daemon_event_tx`, `preapproved`) and helper methods `notify_daemon_accept_state`, `notify_daemon_connection_closed`. `init_ptc_peer_connection` (line 917) consumes preapproved → builds initial `SignalingState` from the recorded fields → proactively sends `AcceptControl` to the peer → emits `ConnectionAcceptStateChanged` to refresh the daemon cache. `handle_request_control` narrow short-circuits via two new pure helpers (`should_short_circuit_control`, `should_short_circuit_clipboard`) and emits `ConnectionAcceptStateChanged` on every commit (accept, release, deny). The `CloseControl` PC-removal site (line 1913) emits `ConnectionClosed`. |

### Test coverage

| Layer | Tests added |
|---|---|
| `ipc-protocol` | Round-trip serde for `ConnectionAcceptStateChanged`, `ConnectionClosed`; `preapproved_connections` round-trip; back-compat decode of legacy `WorkerInitPayload`; `ConnectionAcceptState::default` is all-false. |
| `daemon::worker_manager` | `track → update → drain` round trip; update-unknown-id no-op; `track` idempotency (re-track keeps existing state); `remove_connection` drops the entry from the next drain. |
| `service::signaling` | Pure-helper short-circuit decision matrix (6 cases): grant + accepted ⇒ short-circuit; grant + not accepted ⇒ no; release + accepted ⇒ no (release path stays authoritative); release + not accepted ⇒ no; clipboard analogous independence cases. |

Aggregate: ipc-protocol 8/8, server lib 118/118, all green.

### Out of scope

- WebRTC PC hot-migration (Arch I or II).
- Private-screen / whiteboard internal state restoration (same mechanism
  applies but requires `SignalingState` extension).
- Multi-session "follow specific user" UI semantics.
- Frontend changes — `hasControl` survives auto-reconnect already; the
  proactive `AcceptControl` from the new worker covers the rest.

## Task list

| # | Status | Subject |
|---|---|---|
| 1 | ✅ | Extend ipc-protocol with `ConnectionAcceptState` + new IPC variants |
| 2 | ✅ | Daemon: replace `active_browser_ids` with `active_connections` map |
| 3 | ✅ | Daemon `signaling_proxy`: handle new IPC variants |
| 4 | ✅ | Worker: consume preapproved + emit state-change/closed messages |
| 5 | ✅ | Build + test + commit (split per plan) |

## Commits

Web submodule:

1. `f70c912` — `fix(build): run npm run build and use absolute dist path`
2. `aebfbd4` — `fix(host_control): deny approval immediately when no Tauri client`
3. `85a7998` — `fix(worker): keep heartbeats flowing during long-running handlers`
4. `250709c` — `feat(ipc-protocol): add ConnectionAcceptState + state-change variants`
5. `c9a622a` — `feat(daemon): cache per-connection accept-state across worker restarts`
6. `b53c9a4` — `feat(worker): restore preapproved accept-state at PC creation`

Parent workspace:

1. `dafe109` — `chore: update web submodule (fix build_debug scripts)`
2. `0cf4a68` — `chore: update web submodule (deny approval when no Tauri client)`
3. `f3c387e` — `chore: update web submodule (decouple worker heartbeat from main loop)`
4. `4cca1f5` — `chore: update web submodule (Arch III: persist accept-state across worker restarts)`

## Verification

### Automated (passing)

- `cargo test -p desk-ipc-protocol --lib` → 8/8.
- `cargo test -p lcxl-remote-desk-server --lib` → 118/118.
- `cargo build -p lcxl-remote-desk-server` → no new warnings.

### Manual end-to-end (pending user validation)

Build via `web/build_debug.ps1` (now actually runs `npm run build`) and
exercise:

1. Browser connects, requests control, user approves on Tauri (with or
   without "remember"). Confirm mouse works on Default desktop.
2. Trigger UAC. While the UAC dialog is up: from the browser, click the
   UAC dialog's Yes / No buttons. **Expected**: clicks land on the UAC
   dialog (input works immediately on Winlogon, no re-prompt on Tauri).
3. After UAC closes, confirm control still works on Default desktop.
4. Repeat with the lock screen (Win+L) — same expectation.
5. Repeat with the watchdog killing the worker (set
   `worker_heartbeat_timeout_secs = 5` in settings, hold a debugger on
   the worker for >5 s) — control resumes after worker restart without
   re-prompting.
6. Browser disconnects entirely (close tab) without a desktop switch in
   flight. **Expected**: worker emits `ConnectionClosed`, daemon removes
   the entry from `active_connections`. Reconnecting later triggers a
   fresh `RequireControl` flow (no stale auto-approval).
7. With multiple browsers connected, only the disconnecting one is
   removed; others retain state.
8. User toggled `CloseControl` before UAC. **Expected**: state =
   `accept_control: false`; after UAC, mouse stays disabled until user
   re-grants.

## Walkthrough notes

- The original Phase 2 plan ("primary stays alive, secondary handles
  Winlogon, browser routing switches") was set aside in favour of Arch
  III after the multi-session analysis showed it doesn't address the
  cross-session restart case. Phase 2 may be revisited as an Arch II
  optimisation if the 1-2 s blackout from Arch III turns out to be
  user-visible.
- The dialog-flash regression reported on 2026-05-02 was a build-script
  problem, not a logic problem. The `setInitData(null)` fix in
  `06243c3` was already correct; only the bundle was stale.
- The clipboard-permission privilege-escalation hazard (issue #1 in
  reviewer feedback) is the load-bearing reason the design landed on
  worker-authoritative IPC events rather than daemon-side signaling
  inference. Worth preserving as a tripwire if a future refactor
  considers parsing signaling on the daemon side.
