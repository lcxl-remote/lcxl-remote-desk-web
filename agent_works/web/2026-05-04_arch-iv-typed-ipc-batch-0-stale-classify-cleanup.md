# Arch IV typed-IPC migration — batch 0: stale classify cleanup

## Goal

Open the typed-IPC migration from the transitional
`ServiceToWorker::SignalingMessage` bridge by clearing one piece of
genuinely stale routing first: `AcceptControl` and `DenyControl` were
classified as worker-owned, but they are reply variants emitted by the
daemon (`pc_manager::handle_require_control` writes them outbound to
the browser, see `pc_manager.rs:1623,1669`). They never travel
inbound. The classification was a leftover from before cut 6 moved
the `RequireControl` handler daemon-side; no one cleaned up the
matching reply variants when the request handler migrated.

A stray inbound `AcceptControl` from the browser (a protocol error)
would be bridged over to the worker, where `DeskSession::handle_message`
has no arm for it and falls through to `_ =>` returning
`UNKNOWN_SIGNALING_TYPE` (see `service/signaling.rs:1903-1918`). That
bounces a confusing error envelope back through the daemon to the
browser. The worker bridge is opaque so the daemon can't even see
what's going on.

This batch is a small, no-IPC-variant-change cleanup that:

1. Reclassifies the two reply variants as daemon-owned in `classify`.
2. Adds them to the `route` swallow list (next to `Answer`, `Init`,
   the `DesktopSwitching/Ready` pair, `FetchConnections`,
   `ConnectionList`, `Heartbeat`).
3. Pins the new behaviour in tests.

It is meant as a warm-up before batches 1–4 start migrating the 20
remaining worker-owned `SignalingType`s to typed event-transport
payloads (display/private-screen, manager-plane, terminal). Those
batches will add new IPC variants and double-sided `match` arms; this
one only moves entries between two existing buckets.

## Scope

### `web/server/src/daemon/signaling_router.rs`

- `classify`: `AcceptControl` and `DenyControl` move from the
  worker-owned arm to a new daemon-owned arm grouped with
  `RequireControl`. The doc-comment explains the rationale (reply
  variants of the RequireControl flow; the worker has no handler).
- `route`: the swallow `match` arm (originally `Answer | Init |
  DesktopSwitching | DesktopReady | FetchConnections | ConnectionList
  | Heartbeat`) gains `AcceptControl | DenyControl`. Inbound copies
  are now `RouteOutcome::HandledByDaemon` with a `log::trace!`
  breadcrumb, instead of `RouteOutcome::ForwardToWorker`.

No new IPC variants. No daemon ↔ worker wire changes. The
`SignalingMessage` bridge keeps carrying the remaining 20
worker-owned types until subsequent batches replace them.

### Tests

All in `signaling_router::tests`:

- `classify_daemon_owned_types`: extended with `AcceptControl`,
  `DenyControl` so the new classification is pinned.
- `classify_worker_owned_types`: those two entries removed —
  symmetric to the above so a regression in either direction is
  caught immediately.
- `route_swallows_daemon_emitted_variants`: extended with the two
  variants; the doc-comment is updated to spell out *why* swallowing
  is the safer choice (worker would only return
  `UNKNOWN_SIGNALING_TYPE` and bounce a confusing error back).
- `route_inbound_accept_control_is_swallowed_not_bridged` (new):
  pins the actual route call with a non-empty `from_connection_id`,
  so a future change that accidentally treats `AcceptControl` as
  worker-bound trips this test rather than only the static
  classification one.

## Out of scope

- The 20 remaining worker-owned `SignalingType`s still flow over the
  raw `SignalingMessage` IPC envelope. Their typed migration is
  batches 1–4 of this plan:
  - batch 1: display / private-screen (5 variants)
  - batch 2: manager plane (6 variants)
  - batch 3: terminal (8 variants)
  - batch 4: delete the `SignalingMessage` bridge itself
- Worker-side dead code: `DeskSession::handle_message`'s `_ =>` arm
  in `service/signaling.rs` is unchanged. It still serves the
  legacy `DeskServer` WS path, so we cannot tighten it without
  touching that code path. The bridge no longer feeds it stray
  `AcceptControl/DenyControl`, which is the user-facing fix.

## Validation

- `cargo test -p lcxl-remote-desk-server --lib`: **231 passed** /
  0 failed (was 230; +1 new test
  `route_inbound_accept_control_is_swallowed_not_bridged`).
- `cargo check -p lcxl-remote-desk-server --all-targets`: clean
  (only pre-existing warnings — same set as the previous PR 7 cuts).
- `cargo clippy -p lcxl-remote-desk-server --all-targets`: blocked
  by the pre-existing `desk-capture-engine` `numframestoread <= 0`
  `clippy::absurd_extreme_comparisons` error documented in the
  earlier PR 7 archive. Untouched by this batch.

## Manual e2e

Not required for batch 0 — the only behavioural change is that a
stray inbound `AcceptControl/DenyControl` (a protocol error in the
first place) is now silently dropped instead of bouncing an
`UNKNOWN_SIGNALING_TYPE` error back. Validation is via the new
`route_inbound_accept_control_is_swallowed_not_bridged` unit test.
The full RequireControl → AcceptControl flow (which still goes
through `pc_manager::handle_require_control`) is untouched and is
covered by the existing `pc_manager` tests
(`handle_require_control_auto_allows_and_emits_accept`,
`handle_require_control_regrant_short_circuits`, etc.).
