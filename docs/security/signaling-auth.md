# Signaling Authentication

The project has connection paths with **different authentication mechanisms** that must never be confused.

| Connection | Authentication |
|---|---|
| Desk Server → Local Signaling | `settings.system.local_signaling_token` (auto-generated; present in any mode that embeds a signaling server: `default`, `signaling`, `service-daemon`) |
| Desk Server → Remote Signaling | `?token=<settings.system.signaling_token>` in the WebSocket URL |
| Desk Server → Manager | `?token=<settings.system.manager_api_token>` in the WebSocket URL |
| Browser → Signaling / Manager | **No token parameter.** Uses Actix-Session cookie authentication only. |

## Role adjudication

The signaling server decides each connection's `remote_desk_type` itself — a client's self-reported role is never trusted:

- A connection presenting a **valid** node token authenticates as its reported role (a desk-server host reports `server`).
- A connection presenting a **non-empty but invalid** token is rejected with **401** — it is *not* silently downgraded to a cookie/Browser session. This lets a client whose token went stale clear it and re-issue, instead of looping as an anonymous Browser.
- A connection with **no token** authenticates via the session cookie and is always a `browser`, regardless of any self-reported role.

The same contract holds on both the open-source signaling server and the enterprise manager.

## Obtaining a host token (`POST /api/tokens`)

A logged-in client that wants to connect as a host (`remote_desk_type=server`) obtains the token it must present via `POST /api/tokens` (called on the session cookie). The endpoint exists with the same request/response shape on both backends, so a client needs no server-type probe:

- **Open-source desk-server** returns the co-located `local_signaling_token` (the same secret the embedded host worker uses). It ignores the request `name` and stores nothing. Registered only in modes with an embedded signaling server (`default` / `signaling` / `service-daemon`); a pure `desk-server` does not offer it.
- **Enterprise manager** mints a per-user token in its token table.

## Notes

- **Browser connections do not carry a token.** They authenticate with the Actix-Session cookie. Route extractors must use `Option<web::Query<VersionInfo>>`, and manager signaling routes are excluded from the global Session middleware.
- The `local_signaling_token` is a host credential. It is masked in logs (the `SystemSettings` `Debug` output redacts it and the other secrets) and is never logged in plaintext.
- Tokens for remote signaling and the manager are passed in the WebSocket URL query string.

These mechanisms are distinct on purpose; mixing them up is a security defect.
