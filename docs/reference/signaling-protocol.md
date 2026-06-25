# Signaling Protocol

Signaling carries SDP / ICE exchange and a set of control messages over WebSocket. The shared protocol models live in `signal-facade/`, and the signaling server itself is in `signal/`.

## Message Types

Signaling messages are modeled as the `SignalingType` enum in `signal-facade/src/model/signal.rs`. Each variant has a **unique integer value** and is handled exhaustively in `handle_message` in `signal/src/service.rs` — there is intentionally **no `_ =>` catch-all**, so the compiler enforces that every type is handled.

## Authentication

Signaling endpoints authenticate differently depending on who connects — see [Signaling Authentication](/security/signaling-auth). In short:

- Desk Server → Signaling uses a token in the WebSocket URL query string.
- Browser → Signaling uses the Actix-Session cookie, with **no token parameter**.

## Adding a New Signaling Type

1. Add a variant (with a unique integer value) to `SignalingType`.
2. Handle it in `handle_message` — add a forwarding branch or a dedicated match arm.
3. Update the frontend: regenerate the client and add an `onMessage` handler in the frontend RTC hook.

See the [Module Map](/reference/modules#adding-a-new-signaling-type) for the cross-cutting checklist.
