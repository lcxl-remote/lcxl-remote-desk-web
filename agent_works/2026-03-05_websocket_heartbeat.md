# WebSocket Heartbeat & Reconnection

## Background

When a reverse proxy (like Nginx) sits in front of the server, WebSocket connections are terminated after `proxy_read_timeout` (default 60s) if there's no data. Since we cannot modify the proxy configuration, application-layer heartbeats are needed.

## Design Decision

Browser JavaScript API **cannot** initiate native WebSocket Ping frames, so we use application-layer heartbeats (custom `SignalingType::Heartbeat = 1`).

## Changes

### Backend

#### `signal-facade/src/model/signal.rs`

- Added `Heartbeat = 1` variant to `SignalingType` enum

#### `signal/src/service.rs`

- Added `SignalingType::Heartbeat` handler: responds with heartbeat immediately

### Frontend

#### `vite-project/src/features/desk/constants.ts`

- Added `SIGNALING_TYPE_CODE_HEARTBEAT = 1`

#### `vite-project/src/features/desk/use-desk-signaling.ts`

- **Heartbeat**: sends every 30s, detects timeout at 60s (2 missed replies)
- **Reconnection**: exponential backoff (1s → 2s → 4s → ... max 30s)
- Heartbeat responses consumed internally, not propagated to consumers
- Intentional close (unmount) suppresses reconnection

#### `vite-project/src/features/terminal/terminal-session.tsx`

- **Heartbeat only**: sends every 30s, no reconnection (terminal state lives on server)
- Heartbeat timer properly cleaned up on unmount

### Not Modified

- `use-file-transfer.ts`: short-lived connection with continuous data, no heartbeat needed

## WebSocket Coverage

| Connection | Heartbeat | Reconnect | Rationale |
|---|---|---|---|
| Main signaling | ✅ 30s | ✅ exp backoff | Long-lived, session recoverable |
| Terminal | ✅ 30s | ❌ | Terminal state is server-side |
| File transfer | ❌ | ❌ | Short-lived with continuous data |

## Verification

- `cargo check --package desk-signal-facade --package desk-signal` ✅
- `npx tsc --noEmit` ✅
