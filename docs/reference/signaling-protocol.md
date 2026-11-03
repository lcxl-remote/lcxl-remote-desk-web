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

## Connection discovery

`GET /api/desk/connections` returns the same `RestResponse` envelope on OSS and Manager: successful discovery has `success: true` and `data: []` for an empty list. A failed read has `success: false` and must not be displayed as an offline device or an empty fleet. Manager reports shared-store unavailability with code `109` and corrupt records or database failures with code `1`. The WebSocket `FetchConnections` operation returns a correlated `ConnectionsFetched` on both success and failure; clients check `response_state` before decoding the list.

## Peer reports

Only `browser` and `server` are signaling roles. Host credentials authenticate `server`; authenticated Cookie identity decides browser authority and display name.

`ConnectionModel` separates `version_info` (API version, build number, commit hash and debug marker), `peer` (role, operating system and display name), `exec_capabilities`, and `ai_assistant_settings`. An absent version report is `null`, and an absent OS report is `Other`. A false debug marker does not prove a release build. Capability and settings checks remain independent of API version.

Node credentials stay in the handshake query and are never part of connection metadata. The private host registration carries the client identity, verified shell names, PTY support, runtime ceiling and assistant settings. Shell names use CSV only in the URL query; public capabilities use an array. Source repository URLs are not reported.

A server connection has a stable `device_id`: OSS uses the persisted client identity, and Manager uses the registered device UUID. This identifier does not authorize access. Per-connection `connection_id` changes after reconnect. Local browser pairing additionally requires the HTTP server's `ServerInfo.local_device_id` to match the target; central services, unavailable info, and unknown identities do not enable pairing.

Browser desktop, file and terminal connections share one client build report. `ServerInfo.ai_assistant.device_settings` declares whether this HTTP host implements device AI settings updates; clients do not infer that capability from the presence of a device ID.
